from __future__ import annotations

import hashlib
import io
import json
import os
import sys
import tarfile
from collections.abc import Callable, Mapping, Sequence
from pathlib import Path

import pytest

import train_runner as tr

CLOCK = 4_000_000_000.0  # after any real file mtime
STEPS = (2, 4)
STRENGTHS = (0.5, 1.0)
PER_RUN = 2


def sha(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def make_tar(path: Path, files: Mapping[str, bytes], top: str | None = None) -> None:
    with tarfile.open(path, "w:gz" if top else "w") as tar:
        for name, data in files.items():
            info = tarfile.TarInfo(f"{top}/{name}" if top else name)
            info.size = len(data)
            tar.addfile(info, io.BytesIO(data))


class Rig:
    """A runner whose subprocesses are simulated by ``fake``."""

    def __init__(self, tmp: Path) -> None:
        self.work = tmp / "train"
        self.models = tmp / "models"
        self.assets = tmp / "assets"
        for d in (self.work / "incoming", self.models, self.assets):
            d.mkdir(parents=True)
        self.paths = tr.Paths(self.work)
        self.commands: list[list[str]] = []
        self.fail_on: str | None = None
        self.cancel_on: str | None = None
        self.model = b"model-bytes"
        (self.models / "m.safetensors").write_bytes(self.model)
        self.dataset = {"a.png": b"png-a", "a.txt": b"trig, a", "b.png": b"png-b", "b.txt": b"t, b"}
        make_tar(self.paths.incoming / "dataset.tar", self.dataset)
        self.trainer_src = tmp / "trainer-src.tar.gz"
        make_tar(self.trainer_src, {"sdxl_train_network.py": b"# trainer"}, top="sd-scripts-abc")
        self.job = {
            "output_name": "lora",
            "model_file": "m.safetensors",
            "model_sha256": sha(self.model),
            "trainer_commit": "a" * 40,
            "trainer_tree_sha256": tr.tree_digest(self.trainer_src),
            "files": [{"file": n, "sha256": sha(d)} for n, d in self.dataset.items()],
            "total_steps": STEPS[-1],
            "checkpoint_steps": list(STEPS),
            "eval_strengths": list(STRENGTHS),
            "eval_images_per_run": PER_RUN,
            "train_toml": "max_train_steps = 4\n",
            "dataset_toml": "[general]\n",
            "eval_prompts": "p --d 1\np --d 2\n",
        }
        self.write_job()
        (self.assets / "train_tokenizers.json").write_text("[]")
        self.states: list[str] = []
        self.runner = tr.Runner(
            self.paths,
            self.assets,
            self.models,
            executor=self.fake,
            fetch_trainer=lambda dest, commit: self.fetch(dest, commit),
            fetch_url=lambda _url, dest: dest.touch(),
            clock=lambda: CLOCK,
            sleep=lambda _s: None,
        )

    def write_job(self) -> None:
        (self.paths.incoming / "job.json").write_text(json.dumps(self.job))

    def fetch(self, dest: Path, commit: str) -> Path:
        assert commit == "a" * 40
        dest.write_bytes(self.trainer_src.read_bytes())
        return dest

    def touch(self, path: Path, data: bytes) -> None:
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(data)
        os.utime(path, (0, 0))  # old enough to pass the mid-write guard

    def fake(
        self,
        cmd: Sequence[str],
        cwd: Path,
        log: Path,
        env: Mapping[str, str],
        on_tick: Callable[[], None],
    ) -> int:
        self.commands.append(list(cmd))
        joined = " ".join(cmd)
        log.parent.mkdir(parents=True, exist_ok=True)
        if self.fail_on and self.fail_on in joined:
            return 3
        if "sdxl_train_network.py" in joined:
            assert cwd == self.paths.trainer
            log.write_text("steps:  50%|##| 2/4 [00:01<00:01, 1.0it/s, avr_loss=0.0900]\r")
            on_tick()
            for step in STEPS:
                self.touch(
                    self.paths.checkpoints / f"lora-step{step:08d}.safetensors", b"w%d" % step
                )
            log.write_text(
                "steps:  50%|##| 2/4 [00:01<00:01, 1.0it/s, avr_loss=0.0900]\r"
                "steps: 100%|##| 4/4 [00:02<00:00, 1.0it/s, avr_loss=0.0800]\r"
            )
        elif "sdxl_gen_img.py" in joined:
            out = Path(cmd[cmd.index("--outdir") + 1])
            if self.cancel_on and self.cancel_on in out.name:
                self.paths.cancel_marker.write_text("x")
                on_tick()  # raises Cancelled
            for i in range(PER_RUN):
                self.touch(out / f"im_{i:06d}.png", f"{out.name}-{i}".encode())
            on_tick()
        return 0


@pytest.fixture
def rig(tmp_path: Path) -> Rig:
    return Rig(tmp_path)


def read_state(rig: Rig) -> dict[str, object]:
    data = json.loads(rig.paths.state.read_text())
    assert isinstance(data, dict)
    return data


def manifest_paths(rig: Rig) -> dict[str, dict[str, object]]:
    listing = json.loads(rig.paths.manifest.read_text())["files"]
    return {f["path"]: f for f in listing}


def test_full_run(rig: Rig) -> None:
    rig.runner.execute()
    state = read_state(rig)
    assert state["stage"] == "done", state
    files = manifest_paths(rig)
    assert "checkpoints/lora-step00000002.safetensors" in files
    assert "checkpoints/lora-step00000004.safetensors" in files
    # base + 2 checkpoints x 2 strengths = 5 sets of 2 images.
    pngs = [p for p in files if p.startswith("evaluation/")]
    assert len(pngs) == 5 * PER_RUN
    assert "evaluation/4-1.0/im_000001.png" in pngs
    assert "loss.csv" in files and "train.toml" in files
    ckpt = files["checkpoints/lora-step00000004.safetensors"]
    assert ckpt["sha256"] == sha(b"w4")
    assert ckpt["bytes"] == 2
    csv = (rig.paths.runtime / "loss.csv").read_text()
    assert "2,0.0900" in csv and "4,0.0800" in csv


def test_stages_use_the_pinned_tools(rig: Rig) -> None:
    rig.runner.execute()
    flat = [" ".join(c) for c in rig.commands]
    assert any("train_bootstrap.lock" in c and "--require-hashes" in c for c in flat)
    assert any(c.endswith(f"python install {tr.PYTHON_VERSION}") for c in flat)
    sync = next(c for c in flat if " pip sync " in c)
    assert "--require-hashes" in sync and tr.TORCH_INDEX in sync
    assert (rig.paths.trainer / "sdxl_train_network.py").is_file()
    train = next(c for c in rig.commands if "sdxl_train_network.py" in " ".join(c))
    assert train[-2:] == ["--config_file", str(rig.paths.runtime / "train.toml")]
    assert (rig.paths.runtime / "train.toml").read_text() == "max_train_steps = 4\n"
    gen = [c for c in rig.commands if "sdxl_gen_img.py" in " ".join(c)]
    assert len(gen) == 5
    # Offline, so the generator needs the pinned tokenizers like the trainer.
    assert all(c[c.index("--tokenizer_cache_dir") + 1] == str(rig.paths.tokenizers) for c in gen)
    assert "--network_module" not in gen[0]
    last = gen[-1]
    assert last[last.index("--network_mul") + 1] == "1.0"
    assert last[last.index("--network_weights") + 1].endswith("lora-step00000004.safetensors")


def test_a_quick_job_tests_only_the_final_checkpoint(rig: Rig) -> None:
    rig.job["eval_checkpoints"] = [STEPS[-1]]
    rig.job["eval_strengths"] = [1.0]
    rig.write_job()
    rig.runner.execute()
    assert read_state(rig)["stage"] == "done"
    gen = [c for c in rig.commands if "sdxl_gen_img.py" in " ".join(c)]
    # The base control and one checkpoint at one strength.
    assert len(gen) == 2
    assert gen[1][gen[1].index("--network_weights") + 1].endswith("lora-step00000004.safetensors")
    pngs = [p for p in manifest_paths(rig) if p.startswith("evaluation/")]
    assert sorted(p.split("/")[1] for p in pngs) == ["4-1.0"] * PER_RUN + ["base"] * PER_RUN
    assert rig.runner.job is not None and rig.runner.job.expected_eval_images() == 2 * PER_RUN


def test_evaluation_checkpoints_must_be_saved_ones(rig: Rig) -> None:
    raw = json.dumps({**rig.job, "eval_checkpoints": [3]})
    with pytest.raises(tr.RunError, match="saved checkpoints"):
        tr.Job.parse(raw, rig.models)


def test_progress_is_reported_while_training(rig: Rig) -> None:
    seen: list[dict[str, object]] = []
    original = rig.runner.set_state

    def spy(stage: str, **extra: object) -> None:
        original(stage, **extra)
        seen.append({"stage": stage, **extra})

    rig.runner.set_state = spy  # type: ignore[method-assign]
    rig.runner.execute()
    assert {"stage": "train", "step": 2, "total": 4, "loss": 0.09} in [
        {k: v for k, v in s.items() if k != "updated"} for s in seen
    ]


def test_a_failing_stage_reports_and_keeps_partial_results(rig: Rig) -> None:
    rig.fail_on = "sdxl_gen_img.py"
    rig.runner.execute()
    state = read_state(rig)
    assert state["stage"] == "failed"
    assert "sdxl_gen_img.py failed (exit 3)" in str(state["error"])
    # Checkpoints were already listed, so the launcher can still fetch them.
    assert "checkpoints/lora-step00000004.safetensors" in manifest_paths(rig)


def test_cancel_stops_and_keeps_results(rig: Rig) -> None:
    rig.cancel_on = "4-0.5"
    rig.runner.execute()
    assert read_state(rig)["stage"] == "cancelled"
    files = manifest_paths(rig)
    assert "checkpoints/lora-step00000004.safetensors" in files
    assert "evaluation/base/im_000000.png" in files
    assert not any(p.startswith("evaluation/4-1.0") for p in files)


def test_bad_model_hash_fails_before_training(rig: Rig) -> None:
    (rig.models / "m.safetensors").write_bytes(b"tampered")
    rig.runner.execute()
    state = read_state(rig)
    assert state["stage"] == "failed" and "checksum" in str(state["error"])
    assert not any("sdxl_train_network.py" in " ".join(c) for c in rig.commands)


def test_a_trainer_that_does_not_match_its_digest_is_rejected(rig: Rig) -> None:
    rig.job["trainer_tree_sha256"] = "0" * 64
    rig.write_job()
    rig.runner.execute()
    state = read_state(rig)
    assert state["stage"] == "failed" and "pinned digest" in str(state["error"])
    assert not rig.paths.trainer.exists()


def test_training_without_a_final_checkpoint_fails(rig: Rig) -> None:
    original = rig.fake

    def no_final(
        cmd: Sequence[str],
        cwd: Path,
        log: Path,
        env: Mapping[str, str],
        on_tick: Callable[[], None],
    ) -> int:
        code = original(cmd, cwd, log, env, on_tick)
        if "sdxl_train_network.py" in " ".join(cmd):
            (rig.paths.checkpoints / "lora-step00000004.safetensors").unlink()
        return code

    rig.runner.run = no_final
    rig.runner.execute()
    assert "final checkpoint" in str(read_state(rig)["error"])


def test_evaluation_resumes_without_redoing_finished_sets(rig: Rig) -> None:
    rig.runner.execute()
    first = len(rig.commands)
    # A restart: the same job, everything on disk, one set half-written.
    half = rig.paths.evaluation / "4-1.0"
    (half / "im_000001.png").unlink()
    rig.commands.clear()
    rig.runner.job = tr.Job.parse((rig.paths.incoming / "job.json").read_text(), rig.models)
    rig.runner.evaluate()
    redone = [c[c.index("--outdir") + 1] for c in rig.commands]
    assert redone == [str(half)]
    assert first > len(rig.commands)


def test_the_manifest_skips_files_that_may_be_mid_write(rig: Rig) -> None:
    fresh = rig.paths.checkpoints / "lora-step00000002.safetensors"
    fresh.parent.mkdir(parents=True)
    fresh.write_bytes(b"partial")
    os.utime(fresh, (CLOCK - 1, CLOCK - 1))
    rig.runner.refresh_manifest()
    assert not rig.paths.manifest.exists()
    os.utime(fresh, (CLOCK - 10, CLOCK - 10))
    rig.runner.refresh_manifest()
    assert "checkpoints/lora-step00000002.safetensors" in manifest_paths(rig)


def test_a_changed_file_is_rehashed(rig: Rig) -> None:
    p = rig.paths.checkpoints / "lora-step00000002.safetensors"
    rig.touch(p, b"one")
    rig.runner.refresh_manifest()
    rig.touch(p, b"three")
    os.utime(p, (5, 5))
    rig.runner.refresh_manifest()
    assert manifest_paths(rig)["checkpoints/lora-step00000002.safetensors"]["sha256"] == sha(
        b"three"
    )


def test_wait_for_start_returns_when_the_marker_appears(rig: Rig) -> None:
    calls = {"n": 0}

    def sleeper(_s: float) -> None:
        calls["n"] += 1
        if calls["n"] == 3:
            rig.paths.start_marker.write_text("go")

    rig.runner.sleep = sleeper
    rig.runner.wait_for_start()
    assert calls["n"] == 3 and read_state(rig)["stage"] == "waiting"


def test_progress_parser() -> None:
    log = (
        "loading 3/3 [00:00]\n"
        "steps:   1%|#| 9/920 [00:02<03:00, 4.0it/s, avr_loss=0.1100]\r"
        "steps:  45%|#| 415/920 [01:02<01:15, 6.7it/s, avr_loss=0.0773]"
    )
    assert tr.parse_progress(log) == (415, 920, 0.0773)
    assert tr.parse_progress("nothing here") is None
    assert tr.parse_progress("steps: 0%| | 0/920 [00:00<?, ?it/s]") == (0, 920, None)


@pytest.mark.parametrize(
    ("change", "message"),
    [
        ({"output_name": "../x"}, "output name"),
        ({"model_file": "a/b.safetensors"}, "model file"),
        ({"model_sha256": "xyz"}, "SHA-256"),
        ({"trainer_commit": "main"}, "commit"),
        ({"files": []}, "file list"),
        ({"files": [{"file": "../a.png", "sha256": "0" * 64}]}, "entry"),
        ({"files": [{"file": "a.exe", "sha256": "0" * 64}]}, "entry"),
        ({"total_steps": 0}, "step counts"),
        ({"checkpoint_steps": [2, 3]}, "final step"),
        ({"eval_strengths": []}, "evaluation"),
        ({"total_steps": "many"}, "not valid"),
    ],
)
def test_job_validation(rig: Rig, change: dict[str, object], message: str) -> None:
    raw = json.dumps({**rig.job, **change})
    with pytest.raises(tr.RunError, match=message):
        tr.Job.parse(raw, rig.models)


def test_job_must_be_json_object(rig: Rig) -> None:
    for raw in ("[]", "not json", "{}"):
        with pytest.raises(tr.RunError):
            tr.Job.parse(raw, rig.models)


def test_dataset_extraction_is_strict(tmp_path: Path) -> None:
    expected = {"a.png": sha(b"A")}
    dest = tmp_path / "out"
    dest.mkdir()
    ok = tmp_path / "ok.tar"
    make_tar(ok, {"a.png": b"A"})
    tr.extract_dataset(ok, dest, expected)
    assert (dest / "a.png").read_bytes() == b"A"

    for name, files, message in [
        ("evil.tar", {"../escape.png": b"A"}, "unexpected entry"),
        ("extra.tar", {"a.png": b"A", "b.png": b"B"}, "unexpected entry"),
        ("wrong.tar", {"a.png": b"changed"}, "changed in transit"),
        ("empty.tar", {}, "missing"),
    ]:
        archive = tmp_path / name
        make_tar(archive, files)
        with pytest.raises(tr.RunError, match=message):
            tr.extract_dataset(archive, dest, expected)


def test_dataset_extraction_refuses_links(tmp_path: Path) -> None:
    archive = tmp_path / "link.tar"
    with tarfile.open(archive, "w") as tar:
        info = tarfile.TarInfo("a.png")
        info.type = tarfile.SYMTYPE
        info.linkname = "/etc/passwd"
        tar.addfile(info)
    with pytest.raises(tr.RunError, match="unexpected entry"):
        tr.extract_dataset(archive, tmp_path, {"a.png": sha(b"A")})


def test_tree_digest_ignores_archive_metadata(tmp_path: Path) -> None:
    files = {"x.py": b"1", "sub/y.py": b"2"}
    a, b, c = (tmp_path / n for n in ("a.tgz", "b.tgz", "c.tgz"))
    make_tar(a, files, top="repo-1")
    make_tar(b, dict(reversed(list(files.items()))), top="other-name")
    make_tar(c, {**files, "x.py": b"changed"}, top="repo-1")
    assert tr.tree_digest(a) == tr.tree_digest(b)
    assert tr.tree_digest(a) != tr.tree_digest(c)


@pytest.mark.skipif(sys.platform == "win32", reason="the runner only runs on Linux")
def test_real_executor_runs_and_stops(tmp_path: Path) -> None:
    log = tmp_path / "logs" / "out.log"
    code = tr.real_executor(
        [sys.executable, "-c", "print('hello')"], tmp_path, log, os.environ, lambda: None
    )
    assert code == 0 and "hello" in log.read_text()

    class Stop(Exception):
        pass

    def stop() -> None:
        raise Stop

    with pytest.raises(Stop):
        tr.real_executor(
            [sys.executable, "-c", "import time; time.sleep(60)"],
            tmp_path,
            log,
            os.environ,
            stop,
        )


def lock_file(tmp_path: Path, files: Mapping[str, bytes]) -> Path:
    lock = tmp_path / "tok.json"
    lock.write_text(
        json.dumps(
            [
                {
                    "repository": "org/model",
                    "revision": "b" * 40,
                    "files": {n: sha(d) for n, d in files.items()},
                }
            ]
        )
    )
    return lock


def test_tokenizers_are_fetched_at_the_locked_revision_and_verified(tmp_path: Path) -> None:
    files = {"vocab.json": b"{}", "merges.txt": b"m"}
    urls: list[str] = []

    def fetch(url: str, dest: Path) -> None:
        urls.append(url)
        dest.parent.mkdir(parents=True, exist_ok=True)
        dest.write_bytes(files[url.rsplit("/", 1)[1]])

    out = tmp_path / "tokenizers"
    tr.fetch_tokenizers(lock_file(tmp_path, files), out, fetch)
    assert (out / "org_model" / "vocab.json").read_bytes() == b"{}"
    assert sorted(urls) == [
        f"https://huggingface.co/org/model/resolve/{'b' * 40}/{n}" for n in sorted(files)
    ]
    # A second pass finds verified files and downloads nothing.
    urls.clear()
    tr.fetch_tokenizers(lock_file(tmp_path, files), out, fetch)
    assert urls == []


def test_a_tampered_tokenizer_is_rejected(tmp_path: Path) -> None:
    def fetch(_url: str, dest: Path) -> None:
        dest.parent.mkdir(parents=True, exist_ok=True)
        dest.write_bytes(b"evil")

    out = tmp_path / "tokenizers"
    with pytest.raises(tr.RunError, match="checksum"):
        tr.fetch_tokenizers(lock_file(tmp_path, {"vocab.json": b"{}"}), out, fetch)
    assert not (out / "org_model" / "vocab.json").exists()
    assert not list(out.rglob("*.part"))


def test_a_malformed_tokenizer_lock_is_rejected(tmp_path: Path) -> None:
    lock = tmp_path / "tok.json"
    lock.write_text(json.dumps([{"repository": "../x", "revision": "b" * 40, "files": {}}]))
    with pytest.raises(tr.RunError, match="lock"):
        tr.fetch_tokenizers(lock, tmp_path / "t", lambda *_: None)
    lock.write_text(json.dumps([{"repository": "a/b", "revision": "main", "files": {}}]))
    with pytest.raises(tr.RunError, match="lock"):
        tr.fetch_tokenizers(lock, tmp_path / "t", lambda *_: None)


def test_downloads_only_from_known_hosts(tmp_path: Path) -> None:
    with pytest.raises(tr.RunError, match="unexpected host"):
        tr.download_url("https://evil.example/x", tmp_path / "x")
    with pytest.raises(tr.RunError, match="unexpected host"):
        tr.download_url("file:///etc/passwd", tmp_path / "x")


def test_bootstrap_fetches_the_real_tokenizer_lock(rig: Rig) -> None:
    """The shipped lock is valid and the runner reads it from the asset dir."""
    shipped = Path(__file__).resolve().parent.parent / "train_tokenizers.json"
    (rig.assets / "train_tokenizers.json").write_bytes(shipped.read_bytes())
    fetched: list[str] = []

    def fetch(url: str, dest: Path) -> None:
        fetched.append(url)
        raise tr.RunError("stop after the first download")

    rig.runner.fetch_url = fetch
    rig.runner.execute()
    assert fetched[0].startswith("https://huggingface.co/openai/clip-vit-large-patch14/resolve/")
    assert "stop after" in str(read_state(rig)["error"])


def test_loss_csv_has_one_row_per_step(rig: Rig) -> None:
    log = rig.paths.runtime / "logs" / "train.log"
    log.parent.mkdir(parents=True)
    bar = "steps:  50%|#| {n}/4 [00:00<00:01, 1.0it/s, avr_loss={loss}]"
    lines = [bar.format(n=1, loss="0.0100"), bar.format(n=1, loss="0.0100")]
    lines += [bar.format(n=2, loss="0.0900"), bar.format(n=2, loss="0.0950")]
    log.write_text(chr(13).join(lines) + chr(13))
    rig.runner.write_loss_csv()
    csv = (rig.paths.runtime / "loss.csv").read_text().splitlines()
    assert csv == ["step,loss", "1,0.0100", "2,0.0950"]


def test_logs_are_listed_so_a_failure_can_be_read(rig: Rig) -> None:
    rig.touch(rig.paths.runtime / "logs" / "evaluate-0.log", b"Traceback ...")
    rig.runner.refresh_manifest()
    assert "logs/evaluate-0.log" in manifest_paths(rig)
