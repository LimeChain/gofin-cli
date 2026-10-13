#!/usr/bin/env python3
"""Synthetic lifecycle regressions; no Anvil, remote RPC or funded operation."""
import importlib.util
import io
import json
from pathlib import Path
from types import SimpleNamespace
import tempfile
import unittest
from unittest.mock import Mock, patch

SPEC = importlib.util.spec_from_file_location("cctp_fork_runner", Path(__file__).with_name("verify-cctp-fork.py"))
RUNNER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(RUNNER)


class CctpForkRunnerTests(unittest.TestCase):
    def pin(self, chain=1):
        return {"chain_id": chain, "block": 42, "hash": "0x" + "12" * 32, "timestamp": 1000}

    def output(self, pin):
        return SimpleNamespace(returncode=0, stdout="CCTP_FORK_PIN=" + json.dumps(pin) + "\n", stderr="")

    def test_exact_network_matrix(self):
        self.assertEqual(set(RUNNER.URLS), {1, 42161, 8453})
        self.assertTrue(all(url.startswith("https://") for url in RUNNER.URLS.values()))

    def test_failed_public_pin_cannot_start_node(self):
        with tempfile.TemporaryDirectory() as tmp, patch.object(RUNNER.subprocess, "run", return_value=SimpleNamespace(returncode=1, stdout="", stderr="archive unavailable")), patch.object(RUNNER, "start_owned_process") as start:
            with self.assertRaisesRegex(RuntimeError, "No fork writes ran"):
                RUNNER.start_fork(1, Path("/synthetic/anvil"), Path(tmp) / "evidence")
            start.assert_not_called()

    def test_wrong_chain_or_malformed_hash_cannot_start_node(self):
        for pin in [self.pin(42161), {**self.pin(), "hash": "0xdeadbeef"}, {**self.pin(), "block": 0}]:
            with self.subTest(pin=pin), tempfile.TemporaryDirectory() as tmp, patch.object(RUNNER.subprocess, "run", return_value=self.output(pin)), patch.object(RUNNER, "start_owned_process") as start:
                with self.assertRaises(RuntimeError):
                    RUNNER.start_fork(1, Path("/synthetic/anvil"), Path(tmp) / "evidence")
                start.assert_not_called()

    def test_unowned_listener_cannot_be_used_and_owned_process_is_stopped(self):
        node = Mock(marker={}, process=Mock(pid=123))
        node.process.poll.return_value = None
        node.stop.return_value = True
        lease = Mock(url="http://127.0.0.1:12345", port=12345)
        with tempfile.TemporaryDirectory() as tmp, patch.object(RUNNER.subprocess, "run", return_value=self.output(self.pin())), patch.object(RUNNER, "start_owned_process", return_value=node), patch.object(RUNNER, "allocate_loopback", return_value=lease), patch.object(RUNNER, "process_matches", return_value=True), patch.object(RUNNER, "listener_owned", return_value=False), patch.object(RUNNER.time, "sleep"), patch.object(RUNNER, "rpc") as rpc:
            with self.assertRaisesRegex(RuntimeError, "not ready"):
                RUNNER.start_fork(1, Path("/synthetic/anvil"), Path(tmp) / "evidence")
            node.stop.assert_called_once()
            node.detach.assert_not_called()
            rpc.assert_not_called()
            lease.release.assert_called_once()

    def test_wrong_fork_hash_is_rejected_and_cleaned_up(self):
        node = Mock(marker={}, process=Mock(pid=123))
        node.process.poll.return_value = None
        node.stop.return_value = True
        metadata = {"chainId": 1, "instanceId": "0x" + "34" * 32, "forkedNetwork": {"forkBlockNumber": 42, "forkBlockHash": "0x" + "ff" * 32}}
        with tempfile.TemporaryDirectory() as tmp, patch.object(RUNNER.subprocess, "run", return_value=self.output(self.pin())), patch.object(RUNNER, "start_owned_process", return_value=node), patch.object(RUNNER, "allocate_loopback", return_value=Mock(url="http://127.0.0.1:12345", port=12345)), patch.object(RUNNER, "process_matches", return_value=True), patch.object(RUNNER, "listener_owned", return_value=True), patch.object(RUNNER.time, "sleep"), patch.object(RUNNER, "rpc", return_value=metadata):
            with self.assertRaisesRegex(RuntimeError, "not ready"):
                RUNNER.start_fork(1, Path("/synthetic/anvil"), Path(tmp) / "evidence")
            node.stop.assert_called_once()

    def test_rpc_requires_matching_id_and_success_result(self):
        for body in [{"jsonrpc": "2.0", "id": 2, "result": {}}, {"jsonrpc": "2.0", "id": 1, "error": {"message": "failed"}}, {"jsonrpc": "2.0", "id": 1}]:
            response = io.BytesIO(json.dumps(body).encode())
            opener = Mock()
            opener.open.return_value = response
            with self.subTest(body=body), patch.object(RUNNER, "build_opener", return_value=opener):
                with self.assertRaises(RuntimeError):
                    RUNNER.rpc("http://127.0.0.1:12345", "anvil_metadata", [])


if __name__ == "__main__":
    unittest.main()
