"""Tuned-kernel tables for the Kromoss trainer.

KohakuMossTrainer and LyCORIS time kernel variants on the first run on a card
(about 6 minutes on a 4090) and keep the winners as small JSON files under
``~/.cache/kohakumoss_kernels`` and ``~/.cache/lycoris_kernels``. The tables
are named by card, CUDA and torch version, so a table for another card is
simply never read. This module restores a bundled archive of them before
training, packs the ones a run produced so they can be added to the bundle,
and merges archives::

    python kernel_cache.py merge OUT.tgz IN.tgz [IN.tgz ...]   (later files win)

Archives hold plain JSON files only, under two roots, with sizes bounded; an
archive that holds anything else is refused whole.
"""

from __future__ import annotations

import gzip
import io
import os
import posixpath
import sys
import tarfile
from pathlib import Path

ROOTS = (
    ".cache/kohakumoss_kernels/devices",
    ".cache/kohakumoss_kernels/tuning",
    ".cache/lycoris_kernels",
)
MAX_FILES = 400
MAX_FILE_BYTES = 1024 * 1024
MAX_TOTAL_BYTES = 8 * 1024 * 1024


class CacheError(Exception):
    pass


def allowed(name: str) -> bool:
    """A relative POSIX path of a ``.json`` file under one of ``ROOTS``."""
    if not name or name.startswith("/") or "\\" in name or "\0" in name:
        return False
    if posixpath.normpath(name) != name or ".." in name.split("/"):
        return False
    return name.endswith(".json") and any(name.startswith(root + "/") for root in ROOTS)


def read_archive(archive: Path) -> dict[str, bytes]:
    """The files of ``archive`` by path. Directory entries are ignored; any other
    entry that isn't an allowed regular file, or a size over the limits, refuses
    the whole archive."""
    files: dict[str, bytes] = {}
    total = 0
    try:
        with tarfile.open(archive) as tar:
            for member in tar.getmembers():
                name = member.name.rstrip("/")
                if member.isdir():
                    if name and not any(
                        r == name or r.startswith(name + "/") or name.startswith(r + "/")
                        for r in (".cache", *ROOTS)
                    ):
                        raise CacheError(f"unexpected folder {member.name!r}")
                    continue
                if not member.isfile() or not allowed(name):
                    raise CacheError(f"unexpected entry {member.name!r}")
                if member.size > MAX_FILE_BYTES:
                    raise CacheError(f"{name} is too big")
                total += member.size
                if len(files) >= MAX_FILES or total > MAX_TOTAL_BYTES:
                    raise CacheError("the archive holds too much")
                handle = tar.extractfile(member)
                if handle is None:
                    raise CacheError(f"unreadable entry {member.name!r}")
                files[name] = handle.read()
    except (tarfile.TarError, OSError, EOFError) as e:
        raise CacheError(f"not a readable archive: {e}") from e
    return files


def write_archive(files: dict[str, bytes], dest: Path) -> None:
    """Same files give the same bytes: sorted, no times, no owners, no folders."""
    raw = io.BytesIO()
    with tarfile.open(fileobj=raw, mode="w", format=tarfile.PAX_FORMAT) as tar:
        for name in sorted(files):
            info = tarfile.TarInfo(name)
            info.size = len(files[name])
            info.mode = 0o644
            info.mtime = 0
            info.uid = info.gid = 0
            info.uname = info.gname = "root"
            tar.addfile(info, io.BytesIO(files[name]))
    gz = io.BytesIO()
    with gzip.GzipFile(fileobj=gz, mode="wb", mtime=0, filename="") as f:
        f.write(raw.getvalue())
    dest.parent.mkdir(parents=True, exist_ok=True)
    tmp = dest.with_name(dest.name + ".part")
    tmp.write_bytes(gz.getvalue())
    os.replace(tmp, dest)


def restore(archive: Path, home: Path) -> int:
    """Put the archive's files under ``home``, never replacing one that exists
    (a table the trainer already wrote is at least as fresh). Returns how many
    files were written."""
    written = 0
    for name, data in read_archive(archive).items():
        target = home / name
        if target.exists():
            continue
        target.parent.mkdir(parents=True, exist_ok=True)
        tmp = target.with_name(target.name + ".part")
        tmp.write_bytes(data)
        os.replace(tmp, target)
        written += 1
    return written


def collect(home: Path) -> dict[str, bytes]:
    """The tables under ``home``, within the same limits an archive must meet."""
    files: dict[str, bytes] = {}
    total = 0
    for root in ROOTS:
        base = home / root
        if not base.is_dir():
            continue
        for path in sorted(base.rglob("*.json")):
            if path.is_symlink() or not path.is_file():
                continue
            name = path.relative_to(home).as_posix()
            size = path.stat().st_size
            if not allowed(name) or size > MAX_FILE_BYTES:
                continue
            total += size
            if len(files) >= MAX_FILES or total > MAX_TOTAL_BYTES:
                return files
            files[name] = path.read_bytes()
    return files


def pack(home: Path, dest: Path) -> int:
    """Write the tables under ``home`` to ``dest``; 0 (and no file) when none."""
    files = collect(home)
    if not files:
        return 0
    write_archive(files, dest)
    return len(files)


def signature(home: Path) -> tuple[tuple[str, int], ...]:
    """Cheap change detector for ``pack``: file names and sizes."""
    out: list[tuple[str, int]] = []
    for root in ROOTS:
        base = home / root
        if base.is_dir():
            for path in sorted(base.rglob("*.json")):
                if path.is_file():
                    out.append((path.relative_to(home).as_posix(), path.stat().st_size))
    return tuple(out)


def merge(sources: list[Path], dest: Path) -> int:
    merged: dict[str, bytes] = {}
    for src in sources:
        merged.update(read_archive(src))
    write_archive(merged, dest)
    return len(merged)


def main(argv: list[str]) -> int:
    if len(argv) < 4 or argv[1] != "merge":
        print(__doc__)
        return 2
    try:
        n = merge([Path(p) for p in argv[3:]], Path(argv[2]))
    except CacheError as e:
        print(f"refused: {e}")
        return 1
    print(f"{n} files -> {argv[2]}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
