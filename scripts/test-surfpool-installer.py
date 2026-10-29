#!/usr/bin/env python3
"""Synthetic compiled-artifact cache tests; no network or Rust build required."""
from concurrent.futures import ThreadPoolExecutor
import hashlib
import importlib.util
import os
from pathlib import Path
import shutil
import tempfile
import threading
import unittest
from unittest.mock import patch


def load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


ROOT = Path(__file__).resolve().parents[1]
installer = load("installer", ROOT / "scripts/e2e/install-surfpool.py")
validator = load("validator", ROOT / "scripts/test-meteora-cli-validator.py")


class CacheTests(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.root = Path(temp.name)
        self.cache = self.root / "cache"
        self.first = self.root / "worktree-a/bin"
        self.second = self.root / "worktree-b/bin"
        self.build = self.enterContext(patch.object(installer, "build", side_effect=self.fake_build))

    def fake_build(self, destination):
        binary = destination / "surfpool"
        binary.write_text("#!/bin/sh\necho 'surfpool 1.5.0'\n")
        binary.chmod(0o755)

    def install(self, destination):
        installer.install(destination, self.cache)

    def artifact(self):
        patch_hash = hashlib.sha256(installer.PATCH.read_bytes()).hexdigest()
        return self.cache / installer.cache_key(patch_hash)

    def test_build_once_across_destinations_and_repeated_install(self):
        self.install(self.first)
        self.install(self.first)
        self.install(self.second)
        self.build.assert_called_once()
        self.assertEqual((self.first / "surfpool-build.txt").read_bytes(),
                         (self.second / "surfpool-build.txt").read_bytes())
        with patch.object(validator.shutil, "which", return_value=str(self.second / "surfpool")):
            validator.validate_surfpool()

    def test_installed_binary_survives_cache_eviction(self):
        self.install(self.first)
        shutil.rmtree(self.cache)
        self.install(self.first)
        self.build.assert_called_once()
        self.install(self.second)
        self.assertEqual(self.build.call_count, 2)

    def test_damaged_install_recovers_without_rebuilding_or_poisoning_cache(self):
        self.install(self.first)
        (self.first / "surfpool").write_text("#!/bin/sh\nexit 1\n")
        self.install(self.first)
        self.build.assert_called_once()
        self.assertEqual((self.first / "surfpool").read_bytes(),
                         (self.artifact() / "surfpool").read_bytes())

    def test_invalid_cache_entries_rebuild(self):
        self.install(self.first)
        artifact = self.artifact()
        for damage in ("hash", "record", "encoding", "missing", "permissions", "version", "execution"):
            with self.subTest(damage=damage):
                binary = artifact / "surfpool"
                if damage == "hash":
                    binary.write_text(binary.read_text() + "# changed\n")
                elif damage == "record":
                    (artifact / "surfpool-build.txt").unlink()
                elif damage == "encoding":
                    (artifact / "surfpool-build.txt").write_bytes(b"\xff")
                elif damage == "missing":
                    binary.unlink()
                elif damage == "permissions":
                    binary.chmod(0o644)
                elif damage == "version":
                    binary.write_text("#!/bin/sh\necho 'surfpool 0.0.0'\n")
                else:
                    binary.write_text("#!/bin/sh\nexit 1\n")
                previous = self.build.call_count
                self.install(self.root / damage)
                self.assertEqual(self.build.call_count, previous + 1)

    def test_changed_build_inputs_select_new_artifact(self):
        self.install(self.first)
        patch_file = self.root / "changed.patch"
        patch_file.write_bytes(installer.PATCH.read_bytes() + b"\n")
        changed_installer = self.root / "installer.py"
        changed_installer.write_bytes(Path(installer.__file__).read_bytes() + b"\n")
        changes = (patch.object(installer, "COMMIT", "new-source"),
                   patch.object(installer, "PATCH", patch_file),
                   patch.object(installer, "__file__", str(changed_installer)),
                   patch.object(installer.platform, "machine", return_value="other-arch"))
        for index, change in enumerate(changes):
            with self.subTest(index=index), change:
                self.install(self.root / f"changed-{index}")
                self.assertEqual(self.build.call_count, index + 2)
        self.install(self.second)
        self.assertEqual(self.build.call_count, 5)

    def test_failed_build_is_not_published_and_retry_succeeds(self):
        def fail(destination):
            self.fake_build(destination)
            raise RuntimeError("interrupted build")

        self.build.side_effect = fail
        with self.assertRaisesRegex(RuntimeError, "interrupted build"):
            self.install(self.first)
        self.assertFalse(self.artifact().exists())
        self.assertFalse((self.first / "surfpool-build.txt").exists())
        self.build.side_effect = self.fake_build
        self.install(self.first)
        self.install(self.second)
        self.assertEqual(self.build.call_count, 2)

    def test_concurrent_installers_build_once(self):
        started, release, contended = threading.Event(), threading.Event(), threading.Event()
        real_flock = installer.fcntl.flock

        def acquire(*args):
            if started.is_set():
                contended.set()
            return real_flock(*args)

        def blocked_build(destination):
            started.set()
            if not release.wait(timeout=5):
                raise RuntimeError("test timed out")
            self.fake_build(destination)

        self.build.side_effect = blocked_build
        self.enterContext(patch.object(installer.fcntl, "flock", side_effect=acquire))
        with ThreadPoolExecutor(max_workers=2) as pool:
            first = pool.submit(self.install, self.first)
            try:
                self.assertTrue(started.wait(timeout=5))
                second = pool.submit(self.install, self.second)
                self.assertTrue(contended.wait(timeout=5))
            finally:
                release.set()
            first.result(timeout=5)
            second.result(timeout=5)
        self.build.assert_called_once()

    def test_cache_location_honors_xdg_override(self):
        with patch.dict(os.environ, {"XDG_CACHE_HOME": str(self.root)}):
            self.assertEqual(installer.default_cache_directory(), self.root / "gofin/surfpool")


if __name__ == "__main__":
    unittest.main()
