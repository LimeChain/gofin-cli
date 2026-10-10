#!/usr/bin/env python3
"""Offline checks for AMM fork ownership and cleanup; no Anvil or credentials."""
import importlib.util
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location("amm_runner", Path(__file__).with_name("verify-amm-fork.py"))
RUNNER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(RUNNER)


class Process:
    def poll(self):
        return None

    def wait(self):
        return 101


class Owned:
    def __init__(self, events, name, stops=True):
        self.process = Process()
        self.events, self.name, self.stops = events, name, stops

    def stop(self):
        self.events.append("stop-" + self.name)
        return self.stops

    def detach(self):
        self.events.append("detach-" + self.name)


class Lease:
    url, port = "http://127.0.0.1:12345", 12345

    def release(self):
        pass


class OwnershipTests(unittest.TestCase):
    def exercise(self, mismatch=False, test_stops=True):
        events = []
        node, test = Owned(events, "node"), Owned(events, "test", test_stops)
        def rpc(url, method, params):
            if method == "eth_getBlockByNumber":
                return {"number": "0x100", "hash": "anchor"}
            return {"chainId": 1, "instanceId": "owned", "forkedNetwork": {
                "forkBlockNumber": 256, "forkBlockHash": "other" if mismatch else "anchor"}}
        with tempfile.TemporaryDirectory() as directory, \
             patch.object(RUNNER, "rpc", side_effect=rpc), \
             patch.object(RUNNER, "allocate_loopback", return_value=Lease()), \
             patch.object(RUNNER, "start_owned_process", side_effect=[node, test]) as start:
            if mismatch or not test_stops:
                with self.assertRaisesRegex(RuntimeError, "anchor mismatch" if mismatch else "cleanup unresolved"):
                    RUNNER.run(1, Path(directory))
            else:
                self.assertEqual(RUNNER.run(1, Path(directory)), 101)
            calls = start.call_args_list
        return events, calls

    def test_anchor_mismatch_refuses_test_and_stops_node(self):
        events, calls = self.exercise(mismatch=True)
        self.assertEqual(events, ["stop-node"])
        self.assertEqual(len(calls), 1)

    def test_failed_test_stops_test_before_node(self):
        events, calls = self.exercise()
        self.assertEqual(events, ["stop-test", "stop-node"])
        self.assertIn("--fork-block-number", calls[0].args[0])
        self.assertEqual(calls[1].kwargs["env"]["FIN_AMM_FORK_INSTANCE"], "owned")

    def test_unresolved_test_keeps_node_for_reconciliation(self):
        events, _ = self.exercise(test_stops=False)
        self.assertEqual(events, ["stop-test", "detach-test", "detach-node"])

    def test_protocol_constants_do_not_add_token_defaults(self):
        for chain in RUNNER.URLS:
            config = RUNNER.protocol_config(chain)
            self.assertEqual(config["router_kind"], "swap_router02")
            self.assertTrue(config["v4"]["router_has_hop_price"])
            self.assertNotIn("token", config)

    def test_ramses_uses_its_reviewed_deployer_and_distinct_router(self):
        config = RUNNER.protocol_config(4663, "ramses")
        self.assertEqual(config["router"].lower(), "0xfcbbe2af83f94e7e2a9c35a535b3a04719afd2ae")
        self.assertEqual(config["pool_deployer"].lower(), "0x4b37359bf291abe8453692db58d515a8b013dca9")
        self.assertNotEqual(config["router"], RUNNER.protocol_config(4663)["router"])
        self.assertNotIn("token", config)
        self.assertNotIn("weth", config)
        with self.assertRaisesRegex(RuntimeError, "Robinhood only"):
            RUNNER.protocol_config(1, "ramses")

    def test_ramses_invalid_scope_refuses_before_any_rpc(self):
        with patch.object(RUNNER, "rpc") as transport, tempfile.TemporaryDirectory() as directory:
            for chain, legacy in [(1, False), (4663, True)]:
                with self.assertRaisesRegex(RuntimeError, "reviewed router"):
                    RUNNER.run(chain, Path(directory), legacy, "ramses")
            transport.assert_not_called()


if __name__ == "__main__":
    unittest.main()
