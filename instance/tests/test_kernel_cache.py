from __future__ import annotations

import io
import json
import tarfile
from pathlib import Path

import pytest
from test_train_runner_moss import MossRig, state

import kernel_cache as kc

CARD = ".cache/kohakumoss_kernels/tuning/NVIDIA_GeForce_RTX_4090-sm89-cu12.6-torch2.14.1/0248051962"
FILES = {
    ".cache/kohakumoss_kernels/devices/NVIDIA_GeForce_RTX_4090-sm89-cu12.6-torch2.14.1.json": b"{}",
    f"{CARD}/quant_fwd.json": b'{"a": 1}',
    ".cache/lycoris_kernels/tuning.json": b"[]",
    ".cache/lycoris_kernels/kernels/sm_89-nvidia-geforce-rtx-4090.json": b'{"entries": []}',
}


def make_home(home: Path, files: dict[str, bytes] = FILES) -> Path:
    for name, data in files.items():
        path = home / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(data)
    return home


def raw_tar(dest: Path, members: list[tuple[str, bytes | None]], link: str | None = None) -> Path:
    """A tar of (name, data) files; data None = a folder. ``link`` adds a symlink member."""
    with tarfile.open(dest, "w:gz") as tar:
        for name, data in members:
            info = tarfile.TarInfo(name)
            if data is None:
                info.type = tarfile.DIRTYPE
                tar.addfile(info)
            else:
                info.size = len(data)
                tar.addfile(info, io.BytesIO(data))
        if link:
            info = tarfile.TarInfo(f"{CARD}/link.json")
            info.type = tarfile.SYMTYPE
            info.linkname = link
            tar.addfile(info)
    return dest


def test_pack_then_restore_round_trips(tmp_path: Path) -> None:
    src = make_home(tmp_path / "src")
    assert kc.pack(src, tmp_path / "out.tgz") == len(FILES)
    dst = tmp_path / "dst"
    assert kc.restore(tmp_path / "out.tgz", dst) == len(FILES)
    for name, data in FILES.items():
        assert (dst / name).read_bytes() == data
    assert not list(dst.rglob("*.part"))


def test_pack_is_deterministic_and_skips_other_files(tmp_path: Path) -> None:
    home = make_home(tmp_path / "h")
    (home / ".cache/kohakumoss_kernels/toolchain").mkdir(parents=True)
    (home / ".cache/kohakumoss_kernels/toolchain/vcvars.json").write_text("{}")
    (home / ".cache/other").mkdir()
    (home / ".cache/other/x.json").write_text("{}")
    (home / f"{CARD}/note.txt").write_text("not json")
    kc.pack(home, tmp_path / "a.tgz")
    kc.pack(home, tmp_path / "b.tgz")
    assert (tmp_path / "a.tgz").read_bytes() == (tmp_path / "b.tgz").read_bytes()
    assert set(kc.read_archive(tmp_path / "a.tgz")) == set(FILES)


def test_nothing_to_pack_writes_nothing(tmp_path: Path) -> None:
    assert kc.pack(tmp_path / "empty", tmp_path / "out.tgz") == 0
    assert not (tmp_path / "out.tgz").exists()


def test_restore_never_replaces_a_table_that_exists(tmp_path: Path) -> None:
    src = make_home(tmp_path / "src")
    kc.pack(src, tmp_path / "out.tgz")
    dst = make_home(tmp_path / "dst", {f"{CARD}/quant_fwd.json": b'{"fresh": true}'})
    assert kc.restore(tmp_path / "out.tgz", dst) == len(FILES) - 1
    assert (dst / f"{CARD}/quant_fwd.json").read_bytes() == b'{"fresh": true}'


@pytest.mark.parametrize(
    "name",
    [
        "../evil.json",
        "/abs/evil.json",
        ".cache/kohakumoss_kernels/tuning/../../../evil.json",
        ".cache/kohakumoss_kernels/tuning/./x.json",
        ".cache/kohakumoss_kernels/tuning/x.py",
        ".cache/kohakumoss_kernels/toolchain/vcvars.json",
        ".cache/kohakumoss_kernels/tuningx/x.json",
        ".cache\\lycoris_kernels\\x.json",
        "evil.json",
    ],
)
def test_unexpected_entries_refuse_the_whole_archive(tmp_path: Path, name: str) -> None:
    archive = raw_tar(tmp_path / "bad.tgz", [(f"{CARD}/ok.json", b"{}"), (name, b"x")])
    home = tmp_path / "home"
    with pytest.raises(kc.CacheError):
        kc.restore(archive, home)
    assert not home.exists()


def test_links_and_odd_folders_are_refused(tmp_path: Path) -> None:
    with pytest.raises(kc.CacheError):
        kc.read_archive(raw_tar(tmp_path / "l.tgz", [], link="/etc/passwd"))
    with pytest.raises(kc.CacheError):
        kc.read_archive(raw_tar(tmp_path / "d.tgz", [("etc/", None)]))


def test_size_limits(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    archive = raw_tar(tmp_path / "a.tgz", [(f"{CARD}/a.json", b"x" * 100)])
    monkeypatch.setattr(kc, "MAX_FILE_BYTES", 50)
    with pytest.raises(kc.CacheError):
        kc.read_archive(archive)
    monkeypatch.setattr(kc, "MAX_FILE_BYTES", 1000)
    monkeypatch.setattr(kc, "MAX_TOTAL_BYTES", 150)
    two = raw_tar(
        tmp_path / "b.tgz", [(f"{CARD}/a.json", b"x" * 100), (f"{CARD}/b.json", b"x" * 100)]
    )
    with pytest.raises(kc.CacheError):
        kc.read_archive(two)
    monkeypatch.setattr(kc, "MAX_TOTAL_BYTES", 10_000)
    monkeypatch.setattr(kc, "MAX_FILES", 1)
    with pytest.raises(kc.CacheError):
        kc.read_archive(two)


def test_not_an_archive_is_a_cache_error(tmp_path: Path) -> None:
    bad = tmp_path / "x.tgz"
    bad.write_bytes(b"nope")
    with pytest.raises(kc.CacheError):
        kc.read_archive(bad)


def test_merge_later_archives_win(tmp_path: Path) -> None:
    one = raw_tar(tmp_path / "1.tgz", [(f"{CARD}/a.json", b"1"), (f"{CARD}/b.json", b"1")])
    two = raw_tar(tmp_path / "2.tgz", [(f"{CARD}/b.json", b"2"), (f"{CARD}/c.json", b"2")])
    assert kc.merge([one, two], tmp_path / "m.tgz") == 3
    assert kc.read_archive(tmp_path / "m.tgz") == {
        f"{CARD}/a.json": b"1",
        f"{CARD}/b.json": b"2",
        f"{CARD}/c.json": b"2",
    }
    assert kc.main(["kernel_cache.py", "merge", str(tmp_path / "m2.tgz"), str(one)]) == 0
    assert kc.main(["kernel_cache.py", "merge", str(tmp_path / "m3.tgz"), str(tmp_path)]) == 1
    assert kc.main(["kernel_cache.py"]) == 2


def test_the_bundled_archive_is_valid_and_matches_the_lock() -> None:
    bundled = Path(__file__).resolve().parents[1] / "moss_kernel_cache.tgz"
    files = kc.read_archive(bundled)
    assert files
    assert all("NVIDIA_GeForce_RTX_4090" in n or n.startswith(".cache/lycoris") for n in files)
    card_file = ".cache/lycoris_kernels/kernels/sm_89-nvidia-geforce-rtx-4090.json"
    stamp = json.loads(files[card_file])["stamp"]
    lock = (bundled.parent / "moss_requirements.lock").read_text()
    # The tables were tuned with the exact versions the lock installs.
    assert f"torch=={stamp['torch']} " in lock
    for pkg in ("lycoris-lora", "triton", "tilelang"):
        assert f"{pkg}=={stamp[pkg]} " in lock, pkg


# ----- through the runner ------------------------------------------------------


@pytest.fixture
def rig(tmp_path: Path) -> MossRig:
    return MossRig(tmp_path)


def test_the_runner_restores_before_training_and_hands_back_new_tables(rig: MossRig) -> None:
    home = rig.assets.parent / "home"
    rig.runner.cache_home = home
    bundle = make_home(rig.assets.parent / "bundle")
    kc.pack(bundle, rig.assets / "moss_kernel_cache.tgz")

    rig.runner.execute()
    assert state(rig)["stage"] == "done", state(rig)
    # The bundled tables were unpacked and logged.
    assert (home / f"{CARD}/quant_fwd.json").read_bytes() == b'{"a": 1}'
    log = (rig.paths.runtime / "logs" / "kernel-cache.log").read_text()
    assert f"restored {len(FILES)}" in log
    # They come back as a result file (the trainer would have added its own).
    listing = json.loads(rig.paths.manifest.read_text())["files"]
    entry = next(f for f in listing if f["path"] == "kernel-cache.tgz")
    assert set(kc.read_archive(rig.paths.runtime / "kernel-cache.tgz")) == set(FILES)
    assert entry["bytes"] == (rig.paths.runtime / "kernel-cache.tgz").stat().st_size


def test_a_bad_bundled_archive_does_not_stop_the_run(rig: MossRig) -> None:
    rig.runner.cache_home = rig.assets.parent / "home"
    (rig.assets / "moss_kernel_cache.tgz").write_bytes(b"corrupt")
    rig.runner.execute()
    assert state(rig)["stage"] == "done", state(rig)
    assert "could not restore" in (rig.paths.runtime / "logs" / "kernel-cache.log").read_text()


def test_no_bundle_and_no_tables_means_no_result_file(rig: MossRig) -> None:
    rig.runner.cache_home = rig.assets.parent / "home"
    rig.runner.execute()
    assert state(rig)["stage"] == "done", state(rig)
    assert not (rig.paths.runtime / "kernel-cache.tgz").exists()
    assert "no bundled" in (rig.paths.runtime / "logs" / "kernel-cache.log").read_text()
