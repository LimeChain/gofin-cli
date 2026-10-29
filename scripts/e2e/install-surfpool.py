#!/usr/bin/env python3
"""Install or reuse the cached, pinned Surfpool build with the receipt-capture fix."""
import argparse
import fcntl
import hashlib
import os
from pathlib import Path
import platform
import shutil
import subprocess
import sys
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


def valid_build(directory, patch_hash):
    record = directory / "surfpool-build.txt"
    try:
        return record.is_file() and metadata(directory, patch_hash) == record.read_text()
    except (OSError, UnicodeError, subprocess.SubprocessError):
        return False


def default_cache_directory():
    base = os.environ.get("XDG_CACHE_HOME")
    if not base:
        base = Path.home() / ("Library/Caches" if sys.platform == "darwin" else ".cache")
    return Path(base) / "gofin" / "surfpool"


def cache_key(patch_hash):
    # No GoFin revision or worktree identity: unrelated changes reuse the build.
    # Separate macOS/libc versions and architectures for native library compatibility.
    os_version = platform.mac_ver()[0].split(".")[0] if sys.platform == "darwin" else platform.libc_ver()
    identity = repr((COMMIT, patch_hash, hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
                     platform.system(), platform.machine(), os_version))
    return hashlib.sha256(identity.encode()).hexdigest()


def build(destination):
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


def install(destination, cache_directory):
    destination.mkdir(parents=True, exist_ok=True)
    patch_hash = hashlib.sha256(PATCH.read_bytes()).hexdigest()
    if valid_build(destination, patch_hash):
        return
    cache_directory.mkdir(parents=True, exist_ok=True)
    key = cache_key(patch_hash)
    cached = cache_directory / key
    # Concurrent installers share one build; a failed build never becomes a hit.
    with (cache_directory / f"{key}.lock").open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        if not valid_build(cached, patch_hash):
            print("Building pinned patched Surfpool (cache miss).", file=sys.stderr)
            with tempfile.TemporaryDirectory(prefix=f"{key}-", dir=cache_directory) as temp:
                staged = Path(temp) / "artifact"
                staged.mkdir()
                build(staged)
                record = metadata(staged, patch_hash)
                if record is None:
                    raise RuntimeError("Surfpool build did not produce the pinned executable")
                (staged / "surfpool-build.txt").write_text(record)
                if cached.exists():
                    shutil.rmtree(cached)
                staged.rename(cached)
        else:
            print("Reusing cached patched Surfpool.", file=sys.stderr)
        # Copy the binary, rather than linking it: edits in one installation must
        # not invalidate the shared artifact used by other worktrees.
        with tempfile.TemporaryDirectory(prefix=".surfpool-", dir=destination) as temp:
            for name in ("surfpool", "surfpool-build.txt"):
                copy = Path(temp) / name
                shutil.copy2(cached / name, copy)
                copy.replace(destination / name)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("directory", type=Path)
    parser.add_argument("--cache-dir", type=Path, default=default_cache_directory(),
                        help="shared compiled-artifact cache (default: user cache directory/gofin/surfpool)")
    args = parser.parse_args()
    destination = args.directory.resolve()
    install(destination, args.cache_dir.resolve())
    print(destination / "surfpool")


if __name__ == "__main__":
    main()
