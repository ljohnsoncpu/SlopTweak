"""Release helpers for .github/workflows/release.yml.

  check-version <tag>      the tag matches every version field in the repo
  latest-json ...          write the updater manifest (tauri-plugin-updater v2)
  sha256sums <files...>    print `sha256  name` lines

Standard library only; runs on the Windows runner's Python.
"""

from __future__ import annotations

import argparse
import base64
import binascii
import datetime as dt
import hashlib
import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
REPO = "ljohnsoncpu/SlopTweak"
SEMVER = re.compile(r"^\d+\.\d+\.\d+$")


def repo_versions(root: Path = ROOT) -> dict[str, str]:
    conf = json.loads((root / "app/src-tauri/tauri.conf.json").read_text(encoding="utf-8"))
    pkg = json.loads((root / "app/package.json").read_text(encoding="utf-8"))
    cargo = (root / "app/src-tauri/Cargo.toml").read_text(encoding="utf-8")
    m = re.search(r'^version = "([^"]+)"', cargo, re.MULTILINE)
    return {
        "tauri.conf.json": conf["version"],
        "package.json": pkg["version"],
        "Cargo.toml": m.group(1) if m else "?",
    }


def check_version(tag: str, root: Path = ROOT) -> str:
    if not tag.startswith("v") or not SEMVER.match(tag[1:]):
        raise SystemExit(f"tag {tag!r} isn't vX.Y.Z")
    want = tag[1:]
    bad = {k: v for k, v in repo_versions(root).items() if v != want}
    if bad:
        raise SystemExit(f"tag {tag} doesn't match {bad}")
    return want


def signed_version(sig_b64: str) -> str | None:
    """The `version:` field of a Tauri updater signature's trusted comment."""
    try:
        text = base64.b64decode(sig_b64, validate=True).decode("utf-8")
    except (binascii.Error, UnicodeDecodeError):
        return None
    for line in text.splitlines():
        if line.startswith("trusted comment: "):
            for field in line.removeprefix("trusted comment: ").split("	"):
                key, _, value = field.partition(":")
                if key == "version":
                    return value
    return None


def latest_json(
    version: str,
    installer: Path,
    signature: Path,
    notes: str,
    pub_date: str,
    base_url: str | None = None,
) -> str:
    base = base_url or f"https://github.com/{REPO}/releases/download/v{version}"
    url = f"{base.rstrip('/')}/{installer.name}"
    sig = signature.read_text(encoding="utf-8").strip()
    if not sig:
        raise SystemExit(f"{signature} is empty")
    if signed_version(sig) != version:
        # The app sets requireSignedVersion and would refuse this update.
        raise SystemExit(f"{signature} is not bound to version {version} (--app-version)")
    entry = {"signature": sig, "url": url}
    manifest = {
        "version": version,
        "notes": notes,
        "pub_date": pub_date,
        # The plugin looks up `windows-x86_64-nsis` first, then `windows-x86_64`.
        "platforms": {"windows-x86_64-nsis": entry, "windows-x86_64": entry},
    }
    return json.dumps(manifest, indent=2) + "\n"


def sha256sums(files: list[Path]) -> str:
    return "".join(f"{hashlib.sha256(f.read_bytes()).hexdigest()}  {f.name}\n" for f in files)


def main(argv: list[str] | None = None) -> None:
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawTextHelpFormatter)
    sub = p.add_subparsers(dest="cmd", required=True)
    c = sub.add_parser("check-version")
    c.add_argument("tag")
    j = sub.add_parser("latest-json")
    j.add_argument("--version", required=True)
    j.add_argument("--installer", type=Path, required=True)
    j.add_argument("--signature", type=Path, required=True)
    j.add_argument("--notes", default="")
    j.add_argument("--out", type=Path, required=True)
    j.add_argument("--base-url", help="download base (local updater tests only)")
    s = sub.add_parser("sha256sums")
    s.add_argument("files", type=Path, nargs="+")
    a = p.parse_args(argv)
    if a.cmd == "check-version":
        print(check_version(a.tag))
    elif a.cmd == "latest-json":
        now = dt.datetime.now(dt.UTC).replace(microsecond=0).isoformat().replace("+00:00", "Z")
        a.out.write_text(
            latest_json(a.version, a.installer, a.signature, a.notes, now, a.base_url),
            encoding="utf-8",
        )
        print(f"wrote {a.out}")
    else:
        sys.stdout.write(sha256sums(a.files))


if __name__ == "__main__":
    main()
