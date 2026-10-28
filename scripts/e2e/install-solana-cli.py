#!/usr/bin/env python3
"""Install the pinned public Agave CLI tools used by Solana fixture setup."""
import argparse
import hashlib
import os
from pathlib import Path
import platform
import subprocess
import tarfile
import tempfile
import urllib.request

VERSION = "v4.3.2"
ASSETS = {
    ("Darwin", "arm64"): ("aarch64-apple-darwin", "5439bd2a42c9e8b39923377361a14c9267f6ce699308f9255fb71c3017af76fd"),
    ("Darwin", "x86_64"): ("x86_64-apple-darwin", "0b814d10d774ea3c0211edd51420444e9aadf8b9d08d35d9417d0bcb74e7a5e4"),
    ("Linux", "x86_64"): ("x86_64-unknown-linux-gnu", "5c9599d7fa397375df69d58e8d4f550ee45463dd6d1ecad9e5a1f053ce8e0933"),
}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("directory", type=Path)
    args = parser.parse_args()
    platform_key = (platform.system(), platform.machine())
    if platform_key not in ASSETS:
        raise SystemExit(f"No pinned Agave CLI release for {platform_key}")
    suffix, expected = ASSETS[platform_key]
    name = f"solana-release-{suffix}.tar.bz2"
    url = f"https://github.com/anza-xyz/agave/releases/download/{VERSION}/{name}"
    args.directory.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryFile() as archive_file:
        digest = hashlib.sha256()
        with urllib.request.urlopen(url, timeout=120) as source:
            while chunk := source.read(1024 * 1024):
                digest.update(chunk)
                archive_file.write(chunk)
        if digest.hexdigest() != expected:
            raise SystemExit("Agave release checksum mismatch")
        archive_file.seek(0)
        with tarfile.open(fileobj=archive_file, mode="r:bz2") as archive:
            for tool in ("solana", "solana-keygen"):
                matches = [entry for entry in archive.getmembers() if entry.isfile() and entry.name.rstrip('/').endswith(f"/bin/{tool}")]
                if len(matches) != 1:
                    raise SystemExit(f"Unexpected Agave archive: {tool}")
                source = archive.extractfile(matches[0])
                if source is None:
                    raise SystemExit(f"Missing Agave executable: {tool}")
                destination = args.directory / tool
                destination.write_bytes(source.read())
                os.chmod(destination, 0o755)
                version = subprocess.check_output([str(destination), "--version"], text=True)
                if "4.3.2" not in version:
                    raise SystemExit(f"Unexpected {tool} version: {version}")
    print(args.directory.resolve())


if __name__ == "__main__":
    main()
