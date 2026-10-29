#!/usr/bin/env python3
"""Regression guards for native Meteora dependency discovery and build startup."""
import copy
import importlib.util
from pathlib import Path
import subprocess
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location(
    "meteora_cli", ROOT / "scripts/test-meteora-cli-validator.py")
meteora = importlib.util.module_from_spec(spec)
spec.loader.exec_module(meteora)


class NativeLoadingTests(unittest.TestCase):
    def manifest(self):
        return {
            "family": "dlmm", "program": "program", "pool": "pool",
            "active_bin": -140, "active_bin_array": -2,
            "mint_a": "mint-a", "mint_b": "mint-b",
            "vault_a": "vault-a", "vault_b": "vault-b",
            "bin_arrays": {"-2": "array"},
            "accounts": ["pool", "array", "mint-a", "mint-b", "vault-a", "vault-b"],
            "assets": [{"mint": "mint-a", "token_program": "token", "decimals": 9}],
        }

    def test_same_graph_accepts_price_movement_within_loaded_array(self):
        before = self.manifest()
        after = copy.deepcopy(before)
        after["active_bin"] = -71
        meteora.validate_discovered_manifest(before, after)
        self.assertEqual(before["active_bin"], -140)
        self.assertEqual(after["active_bin"], -71)

    def test_array_crossing_fails_even_if_other_accounts_are_unchanged(self):
        before = self.manifest()
        after = copy.deepcopy(before)
        after.update(active_bin=-70, active_bin_array=-1)
        with self.assertRaisesRegex(RuntimeError, "active_bin_array"):
            meteora.validate_discovered_manifest(before, after)

    def test_every_other_identity_graph_or_asset_field_is_bound(self):
        before = self.manifest()
        for key in before.keys() - {"active_bin"}:
            with self.subTest(key=key):
                after = copy.deepcopy(before)
                after[key] = "changed"
                with self.assertRaises(RuntimeError):
                    meteora.validate_discovered_manifest(before, after)
        after = copy.deepcopy(before)
        after["unexpected"] = None
        with self.assertRaisesRegex(RuntimeError, "unexpected"):
            meteora.validate_discovered_manifest(before, after)

    def test_missing_or_inconsistent_bin_metadata_cannot_pass(self):
        for field in ("active_bin", "active_bin_array"):
            for value in (None, True, "-2", 0):
                with self.subTest(field=field, value=value):
                    before = self.manifest()
                    after = copy.deepcopy(before)
                    after[field] = value
                    with self.assertRaises(RuntimeError):
                        meteora.validate_discovered_manifest(before, after)

    def test_damm_has_no_mutable_metadata_exception(self):
        before = self.manifest()
        before["family"] = "damm_v2"
        after = copy.deepcopy(before)
        after["active_bin"] += 1
        with self.assertRaisesRegex(RuntimeError, "active_bin"):
            meteora.validate_discovered_manifest(before, after)

    def test_only_startup_uses_build_timeout(self):
        success = subprocess.CompletedProcess([], 0, stdout='{"ok": true}', stderr="")
        with patch.object(meteora.subprocess, "run", return_value=success) as run:
            meteora.start_environment({}, "synthetic-test")
            self.assertEqual(run.call_args.kwargs["timeout"], 600)
            meteora.cli({}, "synthetic-test", "status", "operation", "--json")
            self.assertEqual(run.call_args.kwargs["timeout"], 180)


if __name__ == "__main__":
    unittest.main(verbosity=2)
