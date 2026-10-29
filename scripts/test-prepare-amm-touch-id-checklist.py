#!/usr/bin/env python3
"""Focused synthetic tests for the unsigned AMM checklist generator."""

from __future__ import annotations

import importlib.util
import contextlib
import datetime
import io
import json
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch


SCRIPT = Path(__file__).with_name("prepare-amm-touch-id-checklist.py")
SPEC = importlib.util.spec_from_file_location("prepare_amm_touch_id_checklist", SCRIPT)
assert SPEC and SPEC.loader
checklist = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = checklist
SPEC.loader.exec_module(checklist)


class RamsesChecklistTests(unittest.TestCase):
    def test_unsigned_ramses_handoff_uses_only_reads_and_previews(self) -> None:
        token = "0x1111111111111111111111111111111111111111"
        owner = "0x2222222222222222222222222222222222222222"
        asset = {"network": "ethereum:4663", "reference": "native", "decimals": 18}
        calls = []
        def transport(gofin, argv, **kwargs):
            raise AssertionError("the generator must use structured GoFin invocation")
        def invoke(gofin, argv, allowed):
            calls.append(argv)
            if argv[:2] == ["read", "wallet.token"]:
                return {"data": {"reference": token, "decimals": 6}}
            if argv[:2] == ["read", "wallet.balances"]:
                return {"resource": f"ethereum:4663:{owner}", "data": []}
            if argv[:2] == ["read", "ramses.quote"]:
                return {"data": {"dex": "ramses", "protocol": "v3", "tick_spacing": 1,
                    "swap_fee_ppm": 100, "input": "1000000000000000", "output": "2495236",
                    "token_in": asset, "token_out": {"reference": token, "decimals": 6},
                    "price_impact_bps": 1, "block_number": 123, "block_hash": "0x" + "a" * 64,
                    "observed_at": datetime.datetime.now(datetime.timezone.utc).isoformat()}}
            if argv[:2] == ["preview", "ramses.swap"]:
                return {"operation": "ramses.swap", "intent": {
                    "debits": [{"amount": {"asset": asset, "atoms": "1000000000000000"}}],
                    "maximum_costs": [{"amount": {"asset": asset, "atoms": "10000000000000"}}]},
                    "valuation": {"movement_usd_micros": "2500000", "maximum_fee_usd_micros": "25000",
                        "prices": [{"asset": asset, "unit_atoms": "1000000000000000000", "usd_micros_per_unit": "2500000000"}]}}
            if argv[:2] == ["plans", "preview"]:
                return {"review_hash": "b" * 64}
            self.fail(f"unexpected command: {argv[:2]}")
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp) / "unsigned"
            argv = ["generator", "--wallet", "synthetic", "--chain-id", "4663", "--token-out", token,
                "--protocol", "v3", "--dex", "ramses", "--tick-spacing", "1", "--fee-cap", "0.0001", "--out-dir", str(out)]
            with patch.object(sys, "argv", argv), patch.object(checklist, "invoke", side_effect=invoke), \
                 patch.object(checklist.subprocess, "run", side_effect=transport), contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(checklist.main(), 0)
            swap = json.loads((out / "swap-input.json").read_text())
            bounds = json.loads((out / "swap-bounds.json").read_text())
            plan = json.loads((out / "plan.json").read_text())
            self.assertEqual(swap["min_output"], "2.48276")  # rounded upward
            self.assertEqual(swap["max_pool_fee_ppm"], 100)
            self.assertEqual(bounds["allowed_fee_tiers"], [])
            self.assertEqual(bounds["minimum_rate"], {"output_atoms": "244533128", "input_atoms": "100000000000000000"})
            self.assertEqual(plan["steps"][0]["operation"], "ramses.swap")
            self.assertIn("gofin execute ramses.swap", (out / "checklist.md").read_text())
            self.assertNotIn("signature", json.dumps(swap))
            self.assertTrue(all(call[0] in {"read", "preview", "plans"} for call in calls))

    def test_unsigned_hosted_uniswap_handoff_uses_only_reads_and_previews(self) -> None:
        token = "0x1111111111111111111111111111111111111111"
        owner = "0x2222222222222222222222222222222222222222"
        asset = {"network": "ethereum:1", "reference": "native", "decimals": 18}
        calls = []
        def transport(gofin, argv, **kwargs):
            raise AssertionError("the generator must use structured GoFin invocation")
        def invoke(gofin, argv, allowed):
            calls.append(argv)
            if argv[:2] == ["read", "wallet.token"]:
                return {"data": {"reference": token, "decimals": 6}}
            if argv[:2] == ["read", "wallet.balances"]:
                return {"resource": f"ethereum:1:{owner}", "data": []}
            if argv[:2] == ["read", "uniswap.quote"]:
                return {"observed_at": datetime.datetime.now(datetime.timezone.utc).isoformat(),
                    "data": {"provider": "uniswap_api_v1", "routing": "CLASSIC", "quote_id": "synthetic",
                    "input": {"asset":asset,"atoms":"1000000000000000","amount":"0.001"},
                    "output": {"asset":{"network":"ethereum:1","reference":token,"decimals":6},
                               "expected":"2.495236","minimum":"2.48276"},
                    "price_impact_bps":1}}
            if argv[:2] == ["preview", "uniswap.swap"]:
                return {"operation": "uniswap.swap", "intent": {
                    "debits": [{"amount": {"asset": asset, "atoms": "1000000000000000"}}],
                    "maximum_costs": [{"amount": {"asset": asset, "atoms": "10000000000000"}}]},
                    "valuation": {"movement_usd_micros": "2500000", "maximum_fee_usd_micros": "25000",
                        "prices": [{"asset": asset, "unit_atoms": "1000000000000000000", "usd_micros_per_unit": "2500000000"}]}}
            if argv[:2] == ["plans", "preview"]:
                return {"review_hash": "b" * 64}
            self.fail(f"unexpected command: {argv[:2]}")
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp) / "unsigned"
            argv = ["generator", "--wallet", "synthetic", "--chain-id", "1", "--token-out", token,
                "--dex", "uniswap", "--route-preference", "auto", "--fee-cap", "0.0001", "--out-dir", str(out)]
            with patch.object(sys, "argv", argv), patch.object(checklist, "invoke", side_effect=invoke), \
                 patch.object(checklist.subprocess, "run", side_effect=transport), contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(checklist.main(), 0)
            swap = json.loads((out / "swap-input.json").read_text())
            bounds = json.loads((out / "swap-bounds.json").read_text())
            plan = json.loads((out / "plan.json").read_text())
            self.assertEqual(swap["min_output"], "2.48276")  # rounded upward
            self.assertEqual(swap["route_preference"], "auto")
            self.assertNotIn("protocol", swap)
            self.assertNotIn("fee_tier", swap)
            self.assertNotIn("allowed_fee_tiers", bounds)
            self.assertEqual(bounds["minimum_rate"], {"output_atoms": "244533128", "input_atoms": "100000000000000000"})
            self.assertEqual(plan["steps"][0]["operation"], "uniswap.swap")
            self.assertIn("gofin execute uniswap.swap", (out / "checklist.md").read_text())
            self.assertNotIn("signature", json.dumps(swap))
            self.assertTrue(all(call[0] in {"read", "preview", "plans"} for call in calls))

    def test_ramses_quote_binds_dynamic_fee_and_exact_spacing(self) -> None:
        quote = {"dex": "ramses", "protocol": "v3", "tick_spacing": 1, "swap_fee_ppm": 1750}
        self.assertEqual(checklist.validate_ramses_quote(quote, 1, None), 1750)
        self.assertEqual(checklist.validate_ramses_quote(quote, 1, 2000), 2000)
        with self.assertRaises(checklist.ChecklistError):
            checklist.validate_ramses_quote(quote, 5, None)
        with self.assertRaises(checklist.ChecklistError):
            checklist.validate_ramses_quote(quote, 1, 1000)

    def test_ramses_uses_distinct_read_and_write_operations(self) -> None:
        self.assertEqual(checklist.operation_names("ramses"), ("ramses.quote", "ramses.swap"))
        self.assertEqual(checklist.operation_names("uniswap"), ("uniswap.quote", "uniswap.swap"))

    def test_cli_accepts_ramses_without_uniswap_fee_selector(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp) / "new-review"
            argv = [
                "prepare-amm-touch-id-checklist.py", "--wallet", "synthetic",
                "--chain-id", "4663", "--token-out", "0x1111111111111111111111111111111111111111",
                "--protocol", "v3", "--dex", "ramses", "--tick-spacing", "1",
                "--fee-cap", "0.001", "--out-dir", str(out),
            ]
            with patch.object(sys, "argv", argv):
                args = checklist.parse_args()
            self.assertEqual(args.dex, "ramses")
            self.assertEqual(args.tick_spacing, 1)
            self.assertIsNone(args.fee_tier)

    def test_cli_rejects_unreviewed_ramses_network_and_spacing(self) -> None:
        common = [
            "prepare-amm-touch-id-checklist.py", "--wallet", "synthetic",
            "--token-out", "0x1111111111111111111111111111111111111111",
            "--protocol", "v3", "--dex", "ramses", "--tick-spacing", "1",
            "--fee-cap", "0.001",
        ]
        for chain, spacing in ((1, "1"), (4663, "7")):
            with tempfile.TemporaryDirectory() as tmp:
                argv = [*common, "--chain-id", str(chain), "--out-dir", str(Path(tmp) / "review")]
                if spacing != "1":
                    argv[argv.index("1", argv.index("--tick-spacing") + 1)] = spacing
                with patch.object(sys, "argv", argv), self.assertRaises(SystemExit):
                    checklist.parse_args()


class V3ForkChecklistTests(unittest.TestCase):
    def argv(self, out, dex="sushi", chain=1, fee=500, protocol="v3"):
        return ["generator", "--wallet", "synthetic", "--chain-id", str(chain), "--token-out",
                "0x1111111111111111111111111111111111111111", "--protocol", protocol,
                "--dex", dex, "--fee-tier", str(fee), "--fee-cap", "0.0001", "--out-dir", str(out)]

    def test_new_forks_use_only_their_read_and_preview_operations(self):
        token, owner = "0x" + "1" * 40, "0x" + "2" * 40
        for dex in ("sushi", "pancakeswap"):
            asset = {"network": "ethereum:1", "reference": "native", "decimals": 18}
            calls = []
            def invoke(gofin, argv, allowed):
                calls.append(argv)
                if argv[:2] == ["read", "wallet.token"]:
                    return {"data": {"reference": token, "decimals": 6}}
                if argv[:2] == ["read", "wallet.balances"]:
                    return {"resource": f"ethereum:1:{owner}", "data": []}
                if argv[:2] == ["read", f"{dex}.quote"]:
                    return {"data": {"dex": dex, "protocol": "v3", "fee_tier": 500,
                        "input": "1000000000000000", "output": "2495236", "token_in": asset,
                        "token_out": {"reference": token, "decimals": 6}, "price_impact_bps": 1,
                        "block_number": 123, "block_hash": "0x" + "a" * 64,
                        "observed_at": datetime.datetime.now(datetime.timezone.utc).isoformat()}}
                if argv[:2] == ["preview", f"{dex}.swap"]:
                    return {"operation": f"{dex}.swap", "intent": {
                        "debits": [{"amount": {"asset": asset, "atoms": "1000000000000000"}}],
                        "maximum_costs": [{"amount": {"asset": asset, "atoms": "10000000000000"}}]},
                        "valuation": {"movement_usd_micros": "2500000", "maximum_fee_usd_micros": "25000",
                            "prices": [{"asset": asset, "unit_atoms": "1000000000000000000", "usd_micros_per_unit": "2500000000"}]}}
                if argv[:2] == ["plans", "preview"]:
                    return {"review_hash": "b" * 64}
                self.fail(f"unexpected command {argv[:2]}")
            with self.subTest(dex=dex), tempfile.TemporaryDirectory() as tmp:
                out = Path(tmp) / "unsigned"
                with patch.object(sys, "argv", self.argv(out, dex)), patch.object(checklist, "invoke", side_effect=invoke), contextlib.redirect_stdout(io.StringIO()):
                    self.assertEqual(checklist.main(), 0)
                swap = json.loads((out / "swap-input.json").read_text())
                bounds = json.loads((out / "swap-bounds.json").read_text())
                plan = json.loads((out / "plan.json").read_text())
                self.assertEqual(swap["fee_tier"], 500)
                self.assertNotIn("tick_spacing", swap)
                self.assertNotIn("v4_pool_key", swap)
                self.assertEqual(bounds["allowed_fee_tiers"], [500])
                self.assertEqual(plan["steps"][0]["operation"], f"{dex}.swap")
                self.assertIn(f"gofin execute {dex}.swap", (out / "checklist.md").read_text())
                self.assertTrue(all(c[0] in {"read", "preview", "plans"} for c in calls))

    def test_cli_accepts_each_reviewed_new_fork_network_and_fee(self):
        for dex, chains in checklist.V3_NETWORKS.items():
            for chain in chains:
                for fee in checklist.FEE_TIERS[dex]:
                    with tempfile.TemporaryDirectory() as tmp, self.subTest(dex=dex, chain=chain, fee=fee):
                        with patch.object(sys, "argv", self.argv(Path(tmp) / "review", dex, chain, fee)):
                            args = checklist.parse_args()
                        self.assertEqual(args.fee_tier, fee)

    def test_cli_rejects_wrong_pool_selectors_network_protocol_and_fee(self):
        for dex, chain, fee, protocol, extra in [
            ("sushi", 4663, 500, "v3", []), ("sushi", 1, 2500, "v3", []),
            ("pancakeswap", 1, 3000, "v3", []), ("pancakeswap", 1, 500, "v4", []),
            ("sushi", 1, 500, "v3", ["--tick-spacing", "1"]),
            ("pancakeswap", 4663, 500, "v3", ["--pool-key", "{}"]),
            ("sushi", 1, 500, "v3", ["--max-pool-fee-ppm", "1000"]),
        ]:
            with tempfile.TemporaryDirectory() as tmp, self.subTest(dex=dex, extra=extra), contextlib.redirect_stderr(io.StringIO()):
                with patch.object(sys, "argv", self.argv(Path(tmp) / "review", dex, chain, fee, protocol) + extra), self.assertRaises(SystemExit):
                    checklist.parse_args()


if __name__ == "__main__":
    unittest.main()
