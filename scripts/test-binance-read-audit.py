#!/usr/bin/env python3
"""Synthetic audit-runner tests; no credentials, real CLI or network calls."""
import contextlib
import importlib.util
import io
from pathlib import Path
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("read_audit", Path(__file__).with_name("audit-binance-reads.py"))
audit = importlib.util.module_from_spec(spec)
spec.loader.exec_module(audit)


class AuditTests(unittest.TestCase):
    def fixture(self, failure=False, extra=False):
        calls = []

        def call(cli, args):
            calls.append(args)
            if args == ["capabilities"]:
                names = audit.PUBLIC | audit.ACCOUNT_READS | audit.TRADING
                operations = [{"name": name, "kind": "read"} for name in names]
                operations.append({"name": "binance.withdraw", "kind": "write"})
                if extra:
                    operations.append({"name": "binance.new-read", "kind": "read"})
                return 0, {"operations": operations}
            if args == ["connections", "list"]:
                return 0, [
                    {"name": "main", "provider": "binance", "role": "main", "parent": None},
                    {"name": "child", "provider": "binance", "role": "sub", "parent": "main"},
                ]
            self.assertEqual(args[0], "read")
            operation = args[1]
            import json
            data = json.loads(args[3])
            if failure and operation == "binance.futures.positions":
                return 1, {"error": {"code": "VENUE_PERMISSION_DENIED"}}
            if operation == "binance.futures.positions":
                return 0, {"positions": [
                    {"symbol": symbol, "positionAmt": "0.000000000000000001"}
                    for symbol in ["BTCUSDT", "币安人生USDT", "BNBUSDT", "SOLUSDT", "ADAUSDT"]
                ]}
            if operation.endswith(".orders"):
                return 0, {"orders": []}
            if operation.endswith(".trades"):
                rows = [{"orderId": "9007199254740993"}] if data["account"] == "child" else []
                return 0, {"trades": rows}
            return 0, {"operation": operation}
        return call, calls

    def test_read_only_dispatch_parent_authority_and_existing_order_ids(self):
        call, calls = self.fixture()
        with patch.object(audit, "call", call), contextlib.redirect_stdout(io.StringIO()):
            result = audit.audit("synthetic-cli", ["main", "child"], "ETHUSDT", "ETH", "ETH")
        self.assertTrue(result["read_only"])
        self.assertEqual({row["operation"] for row in result["records"]}, audit.PUBLIC | audit.ACCOUNT_READS | audit.TRADING)
        import json
        reads = [(args[1], json.loads(args[3])) for args in calls if args[0] == "read"]
        for operation, data in reads:
            self.assertNotEqual(operation, "binance.withdraw")
            if operation == "binance.hierarchy":
                self.assertEqual(data["authority"], "main")
            if operation.endswith(".order"):
                self.assertEqual(data["order_id"], "9007199254740993")
                self.assertEqual(data["account"], "child")
        symbols = {data["symbol"] for op, data in reads if op == "binance.futures.trades"}
        self.assertEqual(symbols, {"ETHUSDT", "BTCUSDT", "币安人生USDT", "BNBUSDT"})
        self.assertTrue(any(row["status"] == "not_run" and row["account"] == "main" for row in result["records"]))

    def test_changed_inventory_fails_before_any_broker_read(self):
        call, calls = self.fixture(extra=True)
        with patch.object(audit, "call", call), self.assertRaises(ValueError):
            audit.audit("synthetic-cli", ["main"], "ETHUSDT", "ETH", "ETH")
        self.assertEqual(calls, [["capabilities"]])

    def test_account_failure_is_recorded_and_does_not_invent_an_order_id(self):
        call, calls = self.fixture(failure=True)
        with patch.object(audit, "call", call), contextlib.redirect_stdout(io.StringIO()):
            result = audit.audit("synthetic-cli", ["main"], "ETHUSDT", "ETH", "ETH")
        failures = [row for row in result["records"] if row["status"] == "failed"]
        self.assertEqual(len(failures), 1)
        self.assertEqual(failures[0]["result"]["error"]["code"], "VENUE_PERMISSION_DENIED")
        self.assertFalse(any(args[0] == "read" and args[1].endswith(".order") for args in calls))

    def test_unenrolled_accounts_fail_before_venue_reads(self):
        call, calls = self.fixture()
        with patch.object(audit, "call", call), self.assertRaises(ValueError):
            audit.audit("synthetic-cli", ["missing"], "ETHUSDT", "ETH", "ETH")
        self.assertFalse(any(args[0] == "read" for args in calls))


if __name__ == "__main__":
    unittest.main()
