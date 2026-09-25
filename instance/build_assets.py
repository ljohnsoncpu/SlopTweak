"""Build the instance asset bundle reproducibly and print its SHA-256.

The bundle (provision.sh, sidecar.py, requirements.txt) is what onstart downloads
and verifies. Same inputs always give the same bytes, so the pinned hash is stable.
"""

from __future__ import annotations

import argparse
import gzip
import hashlib
import io
import tarfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
FILES = {"provision.sh": 0o755, "sidecar.py": 0o644, "requirements.txt": 0o644}
ONSTART_LIMIT = 4048


def build(out: Path) -> str:
    raw = io.BytesIO()
    with tarfile.open(fileobj=raw, mode="w", format=tarfile.PAX_FORMAT) as tar:
        for name, mode in sorted(FILES.items()):
            data = (HERE / name).read_bytes().replace(b"\r\n", b"\n")
            info = tarfile.TarInfo(name)
            info.size = len(data)
            info.mode = mode
            info.mtime = 0
            info.uid = info.gid = 0
            info.uname = info.gname = "root"
            tar.addfile(info, io.BytesIO(data))
    gz = io.BytesIO()
    with gzip.GzipFile(fileobj=gz, mode="wb", mtime=0, filename="") as f:
        f.write(raw.getvalue())
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_bytes(gz.getvalue())
    return hashlib.sha256(gz.getvalue()).hexdigest()


def onstart_script() -> str:
    script = (HERE / "onstart.sh").read_text().replace("\r\n", "\n")
    if len(script) >= ONSTART_LIMIT:
        raise SystemExit(f"onstart.sh is {len(script)} chars; Vast's limit is {ONSTART_LIMIT}")
    return script


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--out", type=Path, default=HERE / "dist" / "instance-assets.tar.gz")
    args = parser.parse_args()
    digest = build(args.out)
    onstart_script()
    print(f"{digest}  {args.out}")


if __name__ == "__main__":
    main()
