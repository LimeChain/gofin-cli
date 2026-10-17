#!/usr/bin/env python3
"""Build and verify fixed-layout release assets; no installation or authorization.

Python 3.9+ and the standard library only. Native macOS bundles retain the
existing signing/installer boundary; Linux ships offline CLI discovery only.
"""
import argparse
import gzip
import hashlib
import io
import json
import os
import platform
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
TARGETS = {
    "aarch64-apple-darwin": ("Darwin", "arm64"),
    "x86_64-apple-darwin": ("Darwin", "x86_64"),
    "x86_64-unknown-linux-gnu": ("Linux", "x86_64"),
}
MAC_FILES = ("gofin", "gofin-broker", "gofin-touch-id", "install.sh", "README.md", "BUILDINFO", "SHA256SUMS")
LINUX_FILES = ("gofin", "README.md", "BUILDINFO", "SHA256SUMS")
VERSION = r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(?:-([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?"
LINUX_README = """GoFin Linux offline discovery bundle

Run ./gofin --help, ./gofin capabilities --json, or ./gofin schema connections.add --json.
This bundle supports offline help, schemas and capability discovery only.
Protected setup, credential enrollment and financial execution are unavailable:
no Linux authorization provider is implemented. Use the macOS bundle for the
protected human installation and fresh per-write Touch ID path.

BUILDINFO records the source, release version and target. SHA256SUMS verifies
these files. Checksums establish integrity against the downloaded manifest;
they are not publisher signatures.
"""


def run(args, root, env=None):
    return subprocess.run(args, cwd=root, check=True, stdout=subprocess.PIPE,
                          text=True, env=env).stdout.strip()


def resolve(root, tag=None, preview=False):
    head = run(["git", "rev-parse", "HEAD"], root)
    if preview:
        # Untagged local previews use Cargo only to generate their default name.
        # Published releases derive their version solely from the Git tag.
        cargo = (root / "Cargo.toml").read_text(encoding="utf-8")
        section = re.search(r"(?ms)^\[workspace\.package\]\s*\n(.*?)(?=^\[|\Z)", cargo)
        versions = re.findall(r'^version\s*=\s*"([^"\n]+)"\s*(?:#.*)?$', section.group(1), re.M) if section else []
        if len(versions) != 1 or not re.fullmatch(VERSION, versions[0]):
            raise ValueError("Cargo workspace.package must contain one supported SemVer version")
        suffix = ".preview." if "-" in versions[0] else "-preview."
        expected_tag = "v" + versions[0] + suffix + head[:12]
        if tag is not None and tag != expected_tag:
            raise ValueError("preview tag must match the workspace version and HEAD commit")
        tag = expected_tag
    match = re.fullmatch("v" + VERSION, tag or "")
    if not match or (match.group(4) and any(part.isdigit() and len(part) > 1 and part[0] == "0"
                                           for part in match.group(4).split("."))):
        raise ValueError("tag must be vMAJOR.MINOR.PATCH with an optional SemVer prerelease")
    commit = head if preview else run(["git", "rev-parse", "--verify", "refs/tags/" + tag + "^{commit}"], root)
    if not re.fullmatch(r"[0-9a-f]{40,64}", commit):
        raise ValueError("invalid resolved source commit")
    if head != commit:
        raise ValueError("checkout HEAD must match the exact release tag commit")
    if run(["git", "status", "--porcelain", "--untracked-files=all"], root):
        raise ValueError("release checkout must be clean")
    return {"tag": tag, "version": tag[1:], "commit": commit,
            "prerelease": bool(match.group(4)), "preview": preview}


def filename(tag, target):
    suffix = "-discovery" if TARGETS[target][0] == "Linux" else ""
    return "gofin-{}-{}{}.tar.gz".format(tag, target, suffix)


def digest(data):
    return hashlib.sha256(data).hexdigest()


def manifest(files):
    return "".join("{}  {}\n".format(digest(data), name) for name, data in files.items()).encode()


def verify_manifest(data, files):
    if data != manifest(files):
        raise ValueError("checksum manifest does not match the exact bundle contents")


def buildinfo(metadata, target):
    info = ("source_commit={}\nsource_state=clean\narchitecture={}\nprofile=release\n"
            "release_version={}\ntarget={}\n".format(metadata["commit"], TARGETS[target][1],
                                                     metadata["tag"], target)).encode()
    return info + (b"preview=true\n" if metadata.get("preview") else b"")


def read_bundle(bundle, expected):
    if bundle.is_symlink() or not bundle.is_dir() or sorted(p.name for p in bundle.iterdir()) != sorted(expected):
        raise ValueError("bundle must contain exactly the fixed release files")
    files = {}
    for name in expected:
        path = bundle / name
        if path.is_symlink() or not path.is_file():
            raise ValueError("bundle entries must be regular files")
        files[name] = path.read_bytes()
    verify_manifest(files.pop("SHA256SUMS"), files)
    return files


def archive_bytes(files):
    output = io.BytesIO()
    with gzip.GzipFile(fileobj=output, mode="wb", filename="", mtime=0) as compressed:
        with tarfile.open(fileobj=compressed, mode="w", format=tarfile.USTAR_FORMAT) as archive:
            directory = tarfile.TarInfo("bundle/")
            directory.type = tarfile.DIRTYPE
            directory.mode = 0o755
            archive.addfile(directory)
            for name, data in files.items():
                entry = tarfile.TarInfo("bundle/" + name)
                entry.size = len(data)
                entry.mode = 0o755 if name in ("gofin", "gofin-broker", "gofin-touch-id", "install.sh") else 0o644
                entry.mtime = 0
                archive.addfile(entry, io.BytesIO(data))
    return output.getvalue()


def verify_archive(path, metadata, target):
    expected = MAC_FILES if TARGETS[target][0] == "Darwin" else LINUX_FILES
    with tarfile.open(path, "r:gz") as archive:
        members = archive.getmembers()
        if ([m.name for m in members] != ["bundle"] + ["bundle/" + name for name in expected]
                or not members[0].isdir() or members[0].mode != 0o755
                or any(not m.isfile() for m in members[1:])):
            raise ValueError("archive must contain only the fixed regular bundle files")
        files = {}
        for member in members[1:]:
            name = member.name.removeprefix("bundle/")
            expected_mode = 0o755 if name in ("gofin", "gofin-broker", "gofin-touch-id", "install.sh") else 0o644
            if member.mode != expected_mode:
                raise ValueError("archive file modes must match executable and data file roles")
            files[name] = archive.extractfile(member).read()
    verify_manifest(files.pop("SHA256SUMS"), files)
    if files["BUILDINFO"] != buildinfo(metadata, target):
        raise ValueError("archive provenance does not match release tag and target")


def build(root, metadata, target, output):
    if (platform.system(), platform.machine()) != TARGETS[target]:
        raise ValueError("target requires its matching native OS and architecture runner")
    output.mkdir(parents=True, exist_ok=True)
    name = filename(metadata["tag"], target)
    if any(path.exists() or path.is_symlink() for path in (output / name, output / (name + ".sha256"))):
        raise ValueError("refusing to overwrite existing release assets")
    with tempfile.TemporaryDirectory(prefix="gofin-release-") as temporary:
        bundle = Path(temporary) / "bundle"
        build_env = dict(os.environ, GOFIN_RELEASE_VERSION=metadata["version"])
        # Only the publication workflow opts release artifacts into analytics.
        # Local previews never inherit that marker, even in a publisher shell.
        if metadata["preview"]:
            build_env.pop("GOFIN_PUBLISHED_RELEASE", None)
        if TARGETS[target][0] == "Darwin":
            run(["/bin/sh", str(root / "scripts/package-rust-macos.sh"), str(bundle), "--verbose"], root, env=build_env)
            files = read_bundle(bundle, MAC_FILES)
            original_info = ("source_commit={}\nsource_state=clean\narchitecture={}\nprofile=release\n"
                             .format(metadata["commit"], TARGETS[target][1])).encode()
            if files["BUILDINFO"] != original_info:
                raise ValueError("native packager source provenance does not match the release")
        else:
            run(["cargo", "build", "--locked", "--release", "--target", target, "-p", "gofin-cli"], root, env=build_env)
            binary = root / "target" / target / "release/gofin"
            if binary.is_symlink() or not binary.is_file():
                raise ValueError("compiler must produce a regular gofin binary")
            files = {"gofin": binary.read_bytes(), "README.md": LINUX_README.encode()}
        files["BUILDINFO"] = buildinfo(metadata, target)
        files["SHA256SUMS"] = manifest(files)
        archive = output / name
        archive.write_bytes(archive_bytes(files))
        verify_archive(archive, metadata, target)
        (output / (name + ".sha256")).write_bytes(manifest({name: archive.read_bytes()}))
    return archive


def assemble(metadata, output, installer):
    if metadata.get("preview"):
        raise ValueError("preview assets cannot be assembled for release publication")
    names = [filename(metadata["tag"], target) for target in TARGETS]
    expected = names + [name + ".sha256" for name in names]
    if sorted(p.name for p in output.iterdir()) != sorted(expected):
        raise ValueError("publish assets must contain exactly three archives and checksum fragments")
    fragments = []
    for target, name in zip(TARGETS, names):
        archive = output / name
        fragment = output / (name + ".sha256")
        if archive.is_symlink() or fragment.is_symlink() or not archive.is_file() or not fragment.is_file():
            raise ValueError("release assets must be regular files")
        verify_archive(archive, metadata, target)
        checksum = fragment.read_bytes()
        verify_manifest(checksum, {name: archive.read_bytes()})
        fragments.append(checksum)
    if installer.is_symlink() or not installer.is_file():
        raise ValueError("installer must be a regular file")
    shutil.copyfile(installer, output / "install.sh")
    (output / "SHA256SUMS").write_bytes(b"".join(fragments) + manifest({"install.sh": installer.read_bytes()}))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--tag")
    parser.add_argument("--preview", action="store_true", help="HEAD artifact build only; no tag or publication")
    parser.add_argument("--source-dir", type=Path, default=REPO)
    parser.add_argument("--output-dir", type=Path)
    parser.add_argument("--target", choices=TARGETS)
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--resolve", action="store_true")
    mode.add_argument("--assemble", action="store_true")
    parser.add_argument("--installer", type=Path)
    args = parser.parse_args()
    if not args.tag and not args.preview:
        parser.error("a release --tag or --preview is required")
    metadata = resolve(args.source_dir.resolve(), args.tag, args.preview)
    if args.resolve:
        if args.target or args.output_dir or args.installer:
            parser.error("--resolve accepts no target, output or installer")
        print(json.dumps(metadata))
    elif args.assemble:
        if not args.output_dir or not args.installer or args.target:
            parser.error("--assemble requires output and installer, without target")
        assemble(metadata, args.output_dir.resolve(), args.installer.resolve())
    else:
        if not args.target or not args.output_dir or args.installer:
            parser.error("build requires target and output, without installer")
        print(build(args.source_dir.resolve(), metadata, args.target, args.output_dir.resolve()))


if __name__ == "__main__":
    try:
        main()
    except (ValueError, OSError, subprocess.CalledProcessError, tarfile.TarError) as error:
        print("Release build failed: {}".format(error), file=sys.stderr)
        sys.exit(1)
