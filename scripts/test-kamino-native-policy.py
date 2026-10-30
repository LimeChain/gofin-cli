#!/usr/bin/env python3
"""Verify the exact policy changes allowed in the owned Kamino fixture."""
import base64
import copy
import importlib.util
from pathlib import Path
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location(
    "kamino_prepare", ROOT / "scripts/e2e/kamino-prepare.py")
kamino = importlib.util.module_from_spec(spec)
spec.loader.exec_module(kamino)


class NativePolicyTests(unittest.TestCase):
    def fixture(self, paused):
        manifest = {
            "market": "synthetic-market",
            "reserves": [{"address": "synthetic-reserve"}],
            "oracle_age_offsets": [5096, 5104],
            "market_policy_layout": {"account_size": 4664, "borrow_disabled_offset": 124},
        }
        accounts = {"synthetic-market": bytearray([17] * 4664),
                    "synthetic-reserve": bytearray([23] * 8624)}
        accounts["synthetic-market"][124] = paused

        def rpc(method, params):
            if method == "getMultipleAccounts":
                return {"value": [{"owner": kamino.KLEND,
                                   "data": [base64.b64encode(accounts[a]).decode(), "base64"]}
                                  for a in params[0]]}
            self.assertEqual(method, "surfnet_setAccount")
            self.assertEqual(set(params[1]), {"data"})
            accounts[params[0]] = bytearray.fromhex(params[1]["data"])
        return manifest, accounts, rpc

    def test_only_market_borrowing_switch_and_oracle_ages_change(self):
        for paused in (0, 1):
            with self.subTest(paused=paused):
                manifest, accounts, rpc = self.fixture(paused)
                expected = copy.deepcopy(accounts)
                expected["synthetic-market"][124] = 0
                for offset in manifest["oracle_age_offsets"]:
                    expected["synthetic-reserve"][offset:offset + 8] = (86400).to_bytes(8, "little")
                with patch.object(kamino, "reserve_dependencies", return_value=copy.deepcopy(manifest)):
                    kamino.apply_policies("borrowing", rpc, manifest)
                self.assertEqual(accounts, expected)
                self.assertEqual(manifest["market_borrowing_policy"], {
                    "environment": "synthetic_surfpool", "market": "synthetic-market",
                    "cloned_borrow_disabled": paused, "local_borrow_disabled": 0,
                })

    def test_layout_change_fails_before_mutation(self):
        manifest, accounts, rpc = self.fixture(1)
        original = copy.deepcopy(accounts)
        changed = copy.deepcopy(manifest)
        changed["market_policy_layout"]["borrow_disabled_offset"] += 1
        with patch.object(kamino, "reserve_dependencies", return_value=changed):
            with self.assertRaisesRegex(RuntimeError, "identity/layout changed"):
                kamino.apply_policies("borrowing", rpc, manifest)
        self.assertEqual(accounts, original)

    def test_inexact_readback_fails(self):
        manifest, accounts, rpc = self.fixture(1)

        def corrupt(method, params):
            result = rpc(method, params)
            if method == "surfnet_setAccount":
                accounts[params[0]][200] ^= 1
            return result

        with patch.object(kamino, "reserve_dependencies", return_value=copy.deepcopy(manifest)):
            with self.assertRaisesRegex(RuntimeError, "did not retain"):
                kamino.apply_policies("borrowing", corrupt, manifest)


if __name__ == "__main__":
    unittest.main(verbosity=2)
