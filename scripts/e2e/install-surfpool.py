#!/usr/bin/env python3
"""Build the pinned Surfpool release with the local receipt-capture fix."""
import argparse
import hashlib
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import zipfile

COMMIT = "86493c4bf716b01d4bedbc531aa9288cb14d5f36"  # v1.5.0
ROOT = Path(__file__).resolve().parents[2]
PATCH = ROOT / "scripts/e2e/surfpool-v1.5.0.patch"


def run(args, **kwargs):
    subprocess.run(args, check=True, **kwargs)


def metadata(destination, patch_hash):
    binary = destination / "surfpool"
    installer_hash = hashlib.sha256(Path(__file__).read_bytes()).hexdigest()
    if not binary.is_file() or not os.access(binary, os.X_OK):
        return None
    version = subprocess.check_output([str(binary), "--version"], text=True).strip()
    if version != "surfpool 1.5.0":
        return None
    binary_hash = hashlib.sha256(binary.read_bytes()).hexdigest()
    return f"commit={COMMIT}\npatch_sha256={patch_hash}\ninstaller_sha256={installer_hash}\nbinary_sha256={binary_hash}\nversion={version}\n"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("directory", type=Path)
    args = parser.parse_args()
    args.directory.mkdir(parents=True, exist_ok=True)
    destination = args.directory.resolve()
    patch_hash = hashlib.sha256(PATCH.read_bytes()).hexdigest()
    record = destination / "surfpool-build.txt"
    try:
        if record.is_file() and metadata(destination, patch_hash) == record.read_text():
            print(destination / "surfpool")
            return
    except (OSError, subprocess.SubprocessError):
        pass
    with tempfile.TemporaryDirectory(prefix="gofin-surfpool-") as temp:
        source = Path(temp) / "source"
        source.mkdir()
        run(["git", "init", "--quiet"], cwd=source)
        run(["git", "fetch", "--quiet", "--depth", "1", "https://github.com/solana-foundation/surfpool.git", COMMIT], cwd=source)
        run(["git", "checkout", "--quiet", "--detach", COMMIT], cwd=source)
        actual = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=source, text=True).strip()
        if actual != COMMIT:
            raise SystemExit("Surfpool source commit mismatch")
        run(["git", "apply", "--check", str(PATCH)], cwd=source)
        run(["git", "apply", str(PATCH)], cwd=source)
        # Studio is disabled by the harness. Avoid upstream's build-time
        # download of an unpinned latest UI release.
        stub = Path(temp) / "studio-stub.zip"
        with zipfile.ZipFile(stub, "w") as archive:
            archive.writestr("_next/.keep", "")
            archive.writestr("index.html", "Studio disabled in GoFin E2E.\n")
        build_env = dict(os.environ, STUDIO_UI_DIST=str(stub))
        # The binary is copied from this checkout's target directory. Ignore
        # an unrelated caller's target override so that path is deterministic.
        build_env.pop("CARGO_TARGET_DIR", None)
        # Let the pinned checkout's rust-toolchain select the compiler even
        # when the caller has selected a different rustup toolchain.
        build_env.pop("RUSTUP_TOOLCHAIN", None)
        run(["cargo", "build", "--locked", "--release", "-p", "surfpool-cli", "--features", "sqlite"], cwd=source, env=build_env)
        binary = source / "target/release/surfpool"
        shutil.copyfile(binary, destination / "surfpool")
        os.chmod(destination / "surfpool", 0o755)
    record.write_text(metadata(destination, patch_hash))
    print(destination / "surfpool")


if __name__ == "__main__":
    main()
