#!/usr/bin/env python3
"""Synthetic release packaging tests: no compiler, install, secrets or network."""
import importlib.util
import io
import subprocess
import tarfile
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location("gofin_release", Path(__file__).with_name("build-release.py"))
release = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(release)
COMMIT = "abcdef0123456789" * 2 + "abcdef01"


class ReleaseTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="gofin-release-test-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name) / "source"
        self.root.mkdir()
        self.output = Path(temporary.name) / "assets"
        self.cargo_version("0.2.0-beta.1")
        self.commands = []
        self.build_versions = []
        self.status = ""
        self.tag_commit = COMMIT
        self.patches = [patch.object(release, "run", self.tool)]
        for item in self.patches:
            item.start()
            self.addCleanup(item.stop)

    def cargo_version(self, version):
        (self.root / "Cargo.toml").write_text(
            '[workspace]\nmembers=[]\n[workspace.package]\nversion = "' + version +
            '"\n[workspace.dependencies]\nwrong={version="9.9.9"}\n')

    def tool(self, args, root, env=None):
        self.assertEqual(root, self.root)
        self.commands.append(args)
        if args[0] in ("cargo", "/bin/sh"):
            self.build_versions.append(env["GOFIN_RELEASE_VERSION"] if env else None)
        if args == ["git", "rev-parse", "HEAD"]:
            return COMMIT
        if args[:3] == ["git", "rev-parse", "--verify"]:
            return self.tag_commit
        if args[:2] == ["git", "status"]:
            return self.status
        if args[0] == "cargo":
            target = args[args.index("--target") + 1]
            binary = self.root / "target" / target / "release/gofin"
            binary.parent.mkdir(parents=True, exist_ok=True)
            binary.write_bytes(b"synthetic compiled CLI fixture; not executable")
            return ""
        if args[:1] == ["/bin/sh"]:
            bundle = Path(args[2])
            bundle.mkdir()
            for name in release.MAC_FILES[:-2]:
                (bundle / name).write_bytes(("synthetic " + name).encode())
            (bundle / "BUILDINFO").write_bytes(
                ("source_commit=" + COMMIT + "\nsource_state=clean\narchitecture=" +
                 release.platform.machine() + "\nprofile=release\n").encode())
            files = {name: (bundle / name).read_bytes() for name in release.MAC_FILES[:-1]}
            (bundle / "SHA256SUMS").write_bytes(release.manifest(files))
            return ""
        self.fail("unexpected tool invocation: " + repr(args))

    def metadata(self):
        return release.resolve(self.root, "v0.2.0-beta.1")

    def build(self, target, output=None, metadata=None):
        system, machine = release.TARGETS[target]
        with patch.object(release.platform, "system", return_value=system), patch.object(release.platform, "machine", return_value=machine):
            return release.build(self.root, metadata or self.metadata(), target, output or self.output)

    def test_resolve_exact_tag_commit_and_prerelease(self):
        metadata = self.metadata()
        self.assertEqual(metadata, {"tag": "v0.2.0-beta.1", "version": "0.2.0-beta.1", "commit": COMMIT,
                                    "prerelease": True, "preview": False})
        self.assertIn(["git", "rev-parse", "--verify", "refs/tags/v0.2.0-beta.1^{commit}"], self.commands)

    def test_reject_version_injection_and_invalid_semver(self):
        for tag in ("v0.2.0-beta.01", "v01.2.0", "--help", "v0.2.0;echo secret", "v0.2.0\n", "v0.2.0-beta/1", "v0.2.0+build"):
            with self.subTest(tag=tag), self.assertRaises(ValueError):
                release.resolve(self.root, tag)
        self.assertFalse(any(args[:3] == ["git", "rev-parse", "--verify"] for args in self.commands))

    def test_preview_version_is_read_only_from_workspace_package(self):
        (self.root / "Cargo.toml").write_text('[package]\nversion="0.2.0-beta.1"\n')
        with self.assertRaises(ValueError):
            release.resolve(self.root, preview=True)

    def test_release_tag_is_independent_of_cargo_version_without_rewriting_files(self):
        lock = self.root / "Cargo.lock"
        lock.write_text('# synthetic lockfile\nversion = 4\n')
        cargo = self.root / "Cargo.toml"
        original = (cargo.read_bytes(), lock.read_bytes())
        for tag, prerelease in [("v3.0.0", False), ("v4.2.0-beta.7", True)]:
            with self.subTest(tag=tag):
                metadata = release.resolve(self.root, tag)
                self.assertEqual(metadata["version"], tag[1:])
                self.assertEqual(metadata["prerelease"], prerelease)
                self.assertEqual(metadata["commit"], COMMIT)
                for target in release.TARGETS:
                    archive = self.build(target, self.output / tag, metadata)
                    release.verify_archive(archive, metadata, target)
                    self.assertIn(tag, archive.name)
                    self.assertIn(("release_version=" + tag + "\n").encode(), release.buildinfo(metadata, target))
                installer = self.root / "install.sh"
                installer.write_text('#!/bin/sh\n# synthetic\n')
                release.assemble(metadata, self.output / tag, installer)
                self.assertIn(tag, (self.output / tag / "SHA256SUMS").read_text())
        self.assertEqual(self.build_versions, ["3.0.0"] * 3 + ["4.2.0-beta.7"] * 3)
        self.assertEqual((cargo.read_bytes(), lock.read_bytes()), original)

    def test_reject_moved_head_and_dirty_checkout(self):
        self.tag_commit = "1" * 40
        with self.assertRaisesRegex(ValueError, "exact release tag"):
            self.metadata()
        self.tag_commit = COMMIT
        self.status = "?? synthetic-untracked.txt"
        with self.assertRaisesRegex(ValueError, "clean"):
            self.metadata()

    def test_stable_version_and_preview_names_provenance(self):
        self.cargo_version("0.2.0")
        self.assertFalse(release.resolve(self.root, "v0.2.0")["prerelease"])
        metadata = release.resolve(self.root, preview=True)
        self.assertEqual(metadata["tag"], "v0.2.0-preview." + COMMIT[:12])
        self.assertTrue(metadata["preview"])
        self.cargo_version("0.2.0-beta.1")
        metadata = release.resolve(self.root, preview=True)
        self.assertEqual(metadata["tag"], "v0.2.0-beta.1.preview." + COMMIT[:12])
        target = "x86_64-unknown-linux-gnu"
        archive = self.build(target, metadata=metadata)
        release.verify_archive(archive, metadata, target)
        self.assertIn(b"preview=true\n", release.buildinfo(metadata, target))
        with self.assertRaisesRegex(ValueError, "preview tag"):
            release.resolve(self.root, "v0.2.0-beta.1.preview.bad", preview=True)
        with self.assertRaisesRegex(ValueError, "cannot be assembled"):
            release.assemble(metadata, self.output, self.root / "install.sh")

    def test_all_targets_fixed_layout_checksums_and_provenance(self):
        metadata = self.metadata()
        for target in release.TARGETS:
            archive = self.build(target)
            expected_name = "gofin-v0.2.0-beta.1-" + target + ("-discovery" if "linux" in target else "") + ".tar.gz"
            self.assertEqual(archive.name, expected_name)
            release.verify_archive(archive, metadata, target)
            self.assertEqual((archive.parent / (archive.name + ".sha256")).read_bytes(), release.manifest({archive.name: archive.read_bytes()}))
            with tarfile.open(archive) as tar:
                expected = release.LINUX_FILES if "linux" in target else release.MAC_FILES
                self.assertEqual([m.name for m in tar], ["bundle"] + ["bundle/" + name for name in expected])
                self.assertTrue(tar.getmembers()[0].isdir())
                self.assertEqual(tar.getmembers()[0].mode, 0o755)
                for member in tar.getmembers()[1:]:
                    self.assertTrue(member.isfile())
                    self.assertEqual((member.uid, member.gid, member.mtime), (0, 0, 0))
                if "linux" in target:
                    self.assertIn(b"no Linux authorization provider", tar.extractfile("bundle/README.md").read())
        cargo = [args for args in self.commands if args[0] == "cargo"]
        self.assertEqual(cargo, [["cargo", "build", "--locked", "--release", "--target", "x86_64-unknown-linux-gnu", "-p", "gofin-cli"]])
        self.assertEqual(len([args for args in self.commands if args[0] == "/bin/sh"]), 2)

    def test_wrong_native_host_refuses_before_build(self):
        metadata = self.metadata()
        with patch.object(release.platform, "system", return_value="Linux"), patch.object(release.platform, "machine", return_value="x86_64"):
            with self.assertRaisesRegex(ValueError, "matching native"):
                release.build(self.root, metadata, "aarch64-apple-darwin", self.output)
        self.assertFalse(any(args[0] in ("cargo", "/bin/sh") for args in self.commands))

    def test_packaging_is_deterministic_and_never_overwrites(self):
        target = "x86_64-unknown-linux-gnu"
        first = self.build(target)
        second = self.build(target, self.output.parent / "second-assets")
        self.assertEqual(first.read_bytes(), second.read_bytes())
        with self.assertRaisesRegex(ValueError, "overwrite"):
            self.build(target)

    def test_existing_dangling_asset_link_is_not_followed(self):
        target = "x86_64-unknown-linux-gnu"
        metadata = self.metadata()
        self.output.mkdir()
        asset = self.output / release.filename(metadata["tag"], target)
        destination = self.root / "must-not-be-created"
        asset.symlink_to(destination)
        with self.assertRaisesRegex(ValueError, "overwrite"):
            self.build(target)
        self.assertFalse(destination.exists())

    def test_native_packager_provenance_must_match_tag_commit(self):
        real_tool = self.tool

        def wrong_provenance(args, root, env=None):
            result = real_tool(args, root, env=env)
            if args[0] == "/bin/sh":
                bundle = Path(args[2])
                (bundle / "BUILDINFO").write_bytes((bundle / "BUILDINFO").read_bytes().replace(b"source_state=clean", b"source_state=modified"))
                files = {name: (bundle / name).read_bytes() for name in release.MAC_FILES[:-1]}
                (bundle / "SHA256SUMS").write_bytes(release.manifest(files))
            return result

        with patch.object(release, "run", wrong_provenance), self.assertRaisesRegex(ValueError, "source provenance"):
            self.build("aarch64-apple-darwin")
        self.assertFalse(list(self.output.glob("*.tar.gz")))

    def test_inner_manifest_rejects_tampered_native_files(self):
        bundle = self.root / "bundle"
        with patch.object(release.platform, "machine", return_value="arm64"):
            self.tool(["/bin/sh", "synthetic-packager", str(bundle)], self.root)
        (bundle / "gofin").write_bytes(b"changed")
        with self.assertRaisesRegex(ValueError, "checksum"):
            release.read_bundle(bundle, release.MAC_FILES)

    def test_bundle_rejects_extra_files_and_symlinks(self):
        bundle = self.root / "bundle"
        with patch.object(release.platform, "machine", return_value="arm64"):
            self.tool(["/bin/sh", "synthetic-packager", str(bundle)], self.root)
        extra = bundle / "secret.txt"
        extra.write_text("synthetic")
        with self.assertRaisesRegex(ValueError, "exactly"):
            release.read_bundle(bundle, release.MAC_FILES)
        extra.unlink()
        (bundle / "gofin").unlink()
        (bundle / "gofin").symlink_to(bundle / "README.md")
        with self.assertRaisesRegex(ValueError, "regular"):
            release.read_bundle(bundle, release.MAC_FILES)

    def test_archive_rejects_traversal_symlinks_duplicates_and_extra_members(self):
        target = "x86_64-unknown-linux-gnu"
        metadata = self.metadata()
        original = self.build(target)
        for bad_name, kind in (("../gofin", tarfile.REGTYPE), ("bundle/gofin", tarfile.SYMTYPE),
                               ("bundle/gofin", tarfile.REGTYPE), ("bundle/secret", tarfile.REGTYPE)):
            with self.subTest(name=bad_name, kind=kind):
                tampered = self.output / "bad.tar.gz"
                with tarfile.open(original) as source, tarfile.open(tampered, "w:gz") as destination:
                    for member in source:
                        destination.addfile(member, source.extractfile(member))
                    entry = tarfile.TarInfo(bad_name)
                    entry.type = kind
                    entry.linkname = "/synthetic"
                    destination.addfile(entry, io.BytesIO(b""))
                with self.assertRaisesRegex(ValueError, "fixed regular"):
                    release.verify_archive(tampered, metadata, target)

    def test_archive_rejects_wrong_commit_target_and_inner_checksum(self):
        target = "x86_64-unknown-linux-gnu"
        metadata = self.metadata()
        archive = self.build(target)
        with self.assertRaisesRegex(ValueError, "provenance"):
            release.verify_archive(archive, dict(metadata, commit="1" * 40), target)
        with self.assertRaisesRegex(ValueError, "fixed regular"):
            release.verify_archive(archive, metadata, "aarch64-apple-darwin")
        with tarfile.open(archive) as tar:
            files = {m.name[7:]: tar.extractfile(m).read() for m in tar if m.isfile()}
        files["gofin"] += b"tamper"
        archive.write_bytes(release.archive_bytes(files))
        with self.assertRaisesRegex(ValueError, "checksum"):
            release.verify_archive(archive, metadata, target)

    def test_archive_rejects_missing_root_and_tampered_modes(self):
        target = "x86_64-unknown-linux-gnu"
        metadata = self.metadata()
        original = self.build(target)
        for change in ("root_missing", "root_mode", "executable_mode", "data_mode"):
            with self.subTest(change=change):
                tampered = self.output / "bad.tar.gz"
                with tarfile.open(original) as source, tarfile.open(tampered, "w:gz") as destination:
                    for member in source:
                        if change == "root_missing" and member.isdir():
                            continue
                        if ((change == "root_mode" and member.isdir())
                                or (change == "executable_mode" and member.name == "bundle/gofin")
                                or (change == "data_mode" and member.name == "bundle/README.md")):
                            member.mode = 0o777
                        destination.addfile(member, source.extractfile(member))
                with self.assertRaises(ValueError):
                    release.verify_archive(tampered, metadata, target)

    def prepare_publish(self):
        metadata = self.metadata()
        for target in release.TARGETS:
            self.build(target)
        installer = self.root / "install.sh"
        installer.write_bytes(b"#!/bin/sh\n# synthetic public bootstrap fixture\n")
        return metadata, installer

    def test_assembly_combines_exact_fragments_and_installer_checksum(self):
        metadata, installer = self.prepare_publish()
        expected = b"".join((self.output / (release.filename(metadata["tag"], target) + ".sha256")).read_bytes()
                            for target in release.TARGETS) + release.manifest({"install.sh": installer.read_bytes()})
        release.assemble(metadata, self.output, installer)
        self.assertEqual((self.output / "SHA256SUMS").read_bytes(), expected)
        self.assertEqual((self.output / "install.sh").read_bytes(), installer.read_bytes())

    def test_assembly_rejects_missing_extra_and_tampered_assets(self):
        metadata, installer = self.prepare_publish()
        extra = self.output / "unexpected.txt"
        extra.write_text("synthetic")
        with self.assertRaisesRegex(ValueError, "exactly three"):
            release.assemble(metadata, self.output, installer)
        extra.unlink()
        fragment = next(self.output.glob("*.sha256"))
        original = fragment.read_bytes()
        fragment.write_bytes(b"0" * 64 + original[64:])
        with self.assertRaisesRegex(ValueError, "checksum"):
            release.assemble(metadata, self.output, installer)
        fragment.unlink()
        with self.assertRaisesRegex(ValueError, "exactly three"):
            release.assemble(metadata, self.output, installer)
        self.assertFalse((self.output / "install.sh").exists())

    def test_tool_wrapper_uses_argv_and_propagates_compiler_failure(self):
        # Restore the real wrapper only inside this test; its subprocess stays synthetic.
        wrapper = importlib.util.module_from_spec(SPEC)
        SPEC.loader.exec_module(wrapper)
        with patch.object(wrapper.subprocess, "run", return_value=subprocess.CompletedProcess([], 0, " output\n")) as tool:
            self.assertEqual(wrapper.run(["git", "argument with $(literal) spaces"], self.root), "output")
            args, kwargs = tool.call_args
            self.assertEqual(args[0], ["git", "argument with $(literal) spaces"])
            self.assertTrue(kwargs["check"])
            self.assertNotIn("shell", kwargs)
        metadata = self.metadata()
        with patch.object(release, "run", side_effect=subprocess.CalledProcessError(1, ["cargo"])), \
             patch.object(release.platform, "system", return_value="Linux"), \
             patch.object(release.platform, "machine", return_value="x86_64"):
            with self.assertRaises(subprocess.CalledProcessError):
                release.build(self.root, metadata, "x86_64-unknown-linux-gnu", self.output)
        self.assertFalse(list(self.output.glob("*.tar.gz")))


if __name__ == "__main__":
    unittest.main()
