from __future__ import annotations

import json
from pathlib import Path

import pytest
import release_files as rf


def test_repo_versions_agree() -> None:
    versions = set(rf.repo_versions().values())
    assert len(versions) == 1, rf.repo_versions()


def test_check_version() -> None:
    v = next(iter(rf.repo_versions().values()))
    assert rf.check_version(f"v{v}") == v
    with pytest.raises(SystemExit):
        rf.check_version(v)
    with pytest.raises(SystemExit):
        rf.check_version("v99.0.0")
    with pytest.raises(SystemExit):
        rf.check_version(f"v{v}-beta")


def test_latest_json(tmp_path: Path) -> None:
    inst = tmp_path / "SlopTweak_0.2.0_x64-setup.exe"
    inst.write_bytes(b"x")
    sig = tmp_path / "s.sig"
    sig.write_text("c2lnbmF0dXJl\n")
    m = json.loads(rf.latest_json("0.2.0", inst, sig, "notes", "2026-09-25T00:00:00Z"))
    assert m["version"] == "0.2.0"
    for key in ("windows-x86_64-nsis", "windows-x86_64"):
        assert m["platforms"][key] == {
            "signature": "c2lnbmF0dXJl",
            "url": "https://github.com/ljohnsoncpu/SlopTweak/releases/download/v0.2.0/"
            "SlopTweak_0.2.0_x64-setup.exe",
        }
    sig.write_text("")
    with pytest.raises(SystemExit):
        rf.latest_json("0.2.0", inst, sig, "", "")


def test_sha256sums(tmp_path: Path) -> None:
    f = tmp_path / "a.bin"
    f.write_bytes(b"abc")
    assert rf.sha256sums([f]) == (
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad  a.bin\n"
    )
