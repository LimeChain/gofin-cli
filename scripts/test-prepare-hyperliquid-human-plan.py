#!/usr/bin/env python3
"""Synthetic, offline tests for the unsigned Hyperliquid plan helper."""

from __future__ import annotations

import importlib.util
import sys
import unittest
from decimal import Decimal
from pathlib import Path


SCRIPT = Path(__file__).with_name("prepare-hyperliquid-human-plan.py")
SPEC = importlib.util.spec_from_file_location("prepare_hyperliquid_human_plan", SCRIPT)
assert SPEC and SPEC.loader
plan = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = plan
SPEC.loader.exec_module(plan)

OWNER = "0x1111111111111111111111111111111111111111"
WALLET = OWNER
TOKEN_USDC = "0x6d1e7cde53ba9467b783cb7c530ce054"


def market(symbol: str, kind: str, coin: str, decimals: int, quote: dict | None = None) -> dict:
    return {
        "symbol": symbol, "kind": kind, "native_coin": coin, "sz_decimals": decimals,
        "is_delisted": False,
        "base_asset": {"reference": "0x0d01dc56dcaaca66ad901c959b4011ec" if kind == "spot" else "ETH"},
        "quote_asset": quote or {"reference": "USDC", "symbol": "USDC", "decimals": 6, "token_index": None},
    }


def synthetic_evidence() -> dict:
    return {
        "_fin": "synthetic-gofin",
        "spot_markets": {"data": [market("HYPE/USDC", "spot", "@107", 2,
            {"reference": TOKEN_USDC, "symbol": "USDC", "decimals": 8, "token_index": 0})]},
        "perp_markets": {"data": [market("ETH/USDC:USDC", "perpetual", "ETH", 4)]},
        "spot_book": {"data": {"coin": "@107", "levels": [[{"px": "85.00", "sz": "100"}], [{"px": "85.01", "sz": "100"}]]}},
        "eth_book": {"data": {"coin": "ETH", "levels": [[{"px": "2000", "sz": "100"}], [{"px": "2001", "sz": "100"}]]}},
        "balances": {"data": {
            "owner": OWNER,
            "spot": {"balances": [
                {"coin": "USDC", "total": "29.10801997", "hold": "0"},
                {"coin": "HYPE", "total": "0.00981102", "hold": "0"},
            ]},
            "perpetual": {"withdrawable": "50"},
        }},
        "positions": {"data": {"assetPositions": []}},
        "open_orders": {"data": []},
        "trading_state": {"data": {"state": {"leverage": {"type": "cross", "value": 20}}}},
        "abstraction": {"data": {"base": "unifiedAccount", "dex": False}},
        "fees": {"data": {"takerFeeRate": "0.0003"}},
    }


class PlanMathTests(unittest.TestCase):
    def test_native_price_and_size_rounding_are_decimal_safe_and_conservative(self) -> None:
        self.assertEqual(plan.round_price(Decimal("10.06005"), 2, price_decimals=8, upward=True), Decimal("10.061"))
        self.assertEqual(plan.round_price(Decimal("9.94505"), 2, price_decimals=8, upward=False), Decimal("9.9450"))
        self.assertEqual(plan.floor_size(Decimal("1.239"), 2), Decimal("1.23"))
        self.assertEqual(plan.floor_size(Decimal("0.00000009"), 4), Decimal("0.0000"))

    def test_starting_base_balance_must_cover_both_worst_case_fee_paths(self) -> None:
        evidence = synthetic_evidence()
        evidence["balances"]["data"]["spot"]["balances"][1]["total"] = "0.001"
        with self.assertRaisesRegex(plan.PlanError, "free HYPE dust is insufficient"):
            plan.build_plan(evidence, "hl", WALLET, Decimal("0.05"), Decimal("0.50"))

    def test_existing_eth_position_or_order_blocks_static_plan(self) -> None:
        evidence = synthetic_evidence()
        evidence["positions"]["data"]["assetPositions"] = [{"position": {"coin": "ETH", "szi": "0.001"}}]
        with self.assertRaisesRegex(plan.PlanError, "ETH already has exposure"):
            plan.build_plan(evidence, "hl", WALLET, Decimal("0.05"), Decimal("0.50"))
        evidence = synthetic_evidence()
        evidence["open_orders"]["data"] = [{"oid": 123}]
        with self.assertRaisesRegex(plan.PlanError, "Open Hyperliquid orders"):
            plan.build_plan(evidence, "hl", WALLET, Decimal("0.05"), Decimal("0.50"))

    def test_partial_or_unresolved_fin_records_block_plan_creation(self) -> None:
        rows = {"data": [{"id": "synthetic-1", "status": "unknown"}]}
        with self.assertRaisesRegex(plan.PlanError, "Unresolved GoFin operation"):
            plan.check_no_pending_operations(rows)
        plan.check_no_pending_operations({"data": [{"id": "done", "status": "completed"}]})

    def test_withdrawal_is_exact_gross_with_maximum_fee_and_minimum_receipt(self) -> None:
        evidence = synthetic_evidence()
        planned, followup = plan.build_plan(evidence, "hl", WALLET, Decimal("0.05"), Decimal("0.50"))
        withdrawal = followup["input"]
        self.assertEqual(withdrawal["amount"], "5")
        self.assertEqual(withdrawal["max_amount_usdc"], "5")
        self.assertEqual(withdrawal["max_fee_usdc"], "1")
        self.assertEqual(withdrawal["address"], WALLET.lower())
        self.assertEqual(followup["minimum_net_receipt_usdc"], "4")
        self.assertEqual(followup["status"], "bounded_proposal_current_fee_unverified")
        self.assertEqual(planned["steps"][-1]["operation"], "hyperliquid.account.withdraw")

    def test_unified_mode_uses_shared_cash_without_class_transfer(self) -> None:
        evidence = synthetic_evidence()
        planned, _ = plan.build_plan(evidence, "hl", WALLET, Decimal("0.05"), Decimal("0.50"))
        self.assertEqual([row["step_id"] for row in planned["steps"][:3]],
                         ["hype-limit-buy", "hype-bounded-market-sell", "set-eth-isolated-1x"])
        self.assertNotIn("hyperliquid.account.transfer", [row["operation"] for row in planned["steps"]])
        self.assertEqual(planned["steps"][-1]["operation"], "hyperliquid.account.withdraw")
        self.assertEqual(planned["steps"][-1]["completion"]["minimum_net_receipt"]["amount"]["atoms"], "4000000")

    def test_plan_contains_fixed_inputs_and_explicit_partial_fill_stop(self) -> None:
        evidence = synthetic_evidence()
        planned, _ = plan.build_plan(evidence, "hl", WALLET, Decimal("0.05"), Decimal("0.50"))
        steps = planned["steps"]
        by_id = {row["step_id"]: row for row in steps}
        self.assertEqual(by_id["hype-limit-buy"]["input"]["size"], by_id["hype-bounded-market-sell"]["input"]["size"])
        self.assertEqual(by_id["hype-limit-buy"]["input"]["max_notional_usdc"], "12")
        self.assertEqual(by_id["hype-limit-buy"]["input"]["order_type"], "limit")
        self.assertLess(Decimal(by_id["hype-limit-buy"]["input"]["min_price"]),
                        Decimal(by_id["hype-limit-buy"]["input"]["limit_price"]))
        self.assertEqual(by_id["hype-limit-buy"]["input"]["limit_price"], by_id["hype-limit-buy"]["input"]["max_price"])
        self.assertEqual(by_id["hype-bounded-market-sell"]["input"]["order_type"], "market")
        self.assertTrue(by_id["eth-perp-reduce-only-close"]["input"]["reduce_only"])
        self.assertEqual(steps[-1]["operation"], "hyperliquid.account.withdraw")
        self.assertEqual(len({row["input"]["request_id"] for row in steps}), len(steps))
        self.assertEqual(by_id["hype-limit-buy"]["completion"]["minimum_net_receipt"]["amount"]["asset"]["reference"],
                         "0x0d01dc56dcaaca66ad901c959b4011ec")
        self.assertGreater(int(by_id["hype-limit-buy"]["completion"]["minimum_net_receipt"]["amount"]["atoms"]), 0)
        self.assertGreater(int(by_id["hype-bounded-market-sell"]["completion"]["minimum_net_receipt"]["amount"]["atoms"]), 0)
        text = plan.handoff(type("Args", (), {"account": "hl", "wallet": "arb", "wallet_address": WALLET, "out_dir": Path("/tmp/handoff")})(), "a" * 64)
        self.assertIn("Stop if either HYPE order has any partial or zero fill", text)
        self.assertIn("inspect the native filled quantity", text)
        self.assertIn("does not prove full restoration", text)

    def test_owner_alias_mismatch_is_a_hard_stop(self) -> None:
        evidence = synthetic_evidence()
        evidence["balances"]["data"]["owner"] = "0x2222222222222222222222222222222222222222"
        with self.assertRaisesRegex(plan.PlanError, "signer owner and reviewed Arbitrum wallet do not match"):
                plan.build_plan(evidence, "hl", WALLET, Decimal("0.05"), Decimal("0.50"))

    def test_plan_has_exact_transfer_and_fee_caps_without_claiming_live_fee_quote(self) -> None:
        planned, followup = plan.build_plan(synthetic_evidence(), "hl", WALLET, Decimal("0.03"), Decimal("0.50"))
        step = planned["steps"][-1]
        self.assertEqual(step["permitted_variations"]["maximum_debits"][0]["amount"]["atoms"], plan.atoms(Decimal("5")))
        self.assertEqual(step["permitted_variations"]["maximum_costs"][0]["amount"]["atoms"], plan.atoms(Decimal("1")))
        self.assertEqual(step["permitted_variations"]["possible_credits"][0]["asset"], plan.USDC_ARB)
        self.assertEqual(followup["status"], "bounded_proposal_current_fee_unverified")

    def test_spot_base_fee_costs_round_up_to_fin_precision(self) -> None:
        self.assertEqual(plan.atoms_ceil(Decimal("0.03") / Decimal("84.57")),
                         str(int((Decimal("0.03") / Decimal("84.57") * Decimal(10) ** 36).to_integral_value(rounding=plan.ROUND_CEILING))))

    def test_cash_or_account_mode_gaps_stop_instead_of_guessing(self) -> None:
        evidence = synthetic_evidence()
        evidence["balances"]["data"]["spot"]["balances"][0]["total"] = "15"
        with self.assertRaisesRegex(plan.PlanError, "unified free USDC cannot cover"):
            plan.build_plan(evidence, "hl", WALLET, Decimal("0.03"), Decimal("0.50"))
        evidence = synthetic_evidence()
        evidence["abstraction"]["data"]["base"] = "portfolioMargin"
        with self.assertRaisesRegex(plan.PlanError, "standard separate or unified"):
            plan.build_plan(evidence, "hl", WALLET, Decimal("0.03"), Decimal("0.50"))

    def test_native_perpetual_usdc_precision_is_six_but_fin_asset_is_normalized_36(self) -> None:
        evidence = synthetic_evidence()
        evidence["perp_markets"]["data"][0]["quote_asset"]["decimals"] = 36
        with self.assertRaisesRegex(plan.PlanError, "verified primary USDC collateral"):
            plan.build_plan(evidence, "hl", WALLET, Decimal("0.03"), Decimal("0.50"))
        accepted, _ = plan.build_plan(synthetic_evidence(), "hl", WALLET, Decimal("0.03"), Decimal("0.50"))
        self.assertEqual(accepted["aggregate_bounds"]["maximum_debits"][0]["amount"]["asset"]["decimals"], 36)

    def test_cumulative_risk_ceiling_and_zero_loss_scope(self) -> None:
        evidence = synthetic_evidence()
        with self.assertRaisesRegex(plan.PlanError, "cumulative fees, spot roundtrip price loss"):
            plan.build_plan(evidence, "hl", WALLET, Decimal("1"), Decimal("1"))
        # Fees + perp loss + withdrawal fee total exactly $5 before the
        # conservative HYPE buy/sell price-gap loss is included.
        with self.assertRaisesRegex(plan.PlanError, "spot roundtrip price loss"):
            plan.build_plan(evidence, "hl", WALLET, Decimal("0.75"), Decimal("0.50"))
        built, _ = plan.build_plan(evidence, "hl", WALLET, Decimal("0.03"), Decimal("0"))
        for row in built["steps"]:
            if row["step_id"].startswith("eth-perp-"):
                self.assertFalse(any(
                    bound["amount"]["atoms"] == "0"
                    for bound in row["permitted_variations"]["maximum_debits"]
                ))
                self.assertFalse(any(
                    bound["amount"]["atoms"] == "0"
                    for bound in row["permitted_variations"]["maximum_costs"]
                ))

    def test_command_allowlist_never_accepts_financial_execution_or_authorization(self) -> None:
        for command in ("execute", "authorize", "install", "start", "stop", "preview"):
            with self.subTest(command=command), self.assertRaises(plan.PlanError):
                plan.invoke("gofin", [command, "anything", "--json"], {"operations", "snapshot", "plans"})

    def test_wallet_balance_actual_wrapper_uses_top_level_resource_with_data_array(self) -> None:
        address = "0xAbC11111111111111111111111111111111111111"
        result = {
            "operation": "wallet.balances",
            "resource": f"ethereum:42161:{address}",
            "chain_id": 42161,
            "data": [{"resource": f"ethereum:42161:{address}", "amount": {"asset": plan.USDC_ARB, "atoms": "30"}}],
        }
        self.assertTrue(plan.wallet_alias_matches(result, address.upper().replace("0X", "0x")))
        self.assertFalse(plan.wallet_alias_matches(result, "0x2222222222222222222222222222222222222222"))


if __name__ == "__main__":
    unittest.main()
