#!/usr/bin/env python3
"""Synthetic failure checks: a bad public pin must never start or mutate a fork."""
import importlib.util
import json
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest import mock

SPEC = importlib.util.spec_from_file_location("base_aave_runner", Path(__file__).with_name("verify-base-aave-fork.py"))
runner = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(runner)


class PinFailureTests(unittest.TestCase):
    def run_failure(self, status, stdout):
        with tempfile.TemporaryDirectory() as directory:
            source = Path(directory)
            anvil = source / "synthetic-anvil"
            anvil.write_text("synthetic test fixture; never executed\n")
            anvil.chmod(0o700)
            completed = subprocess.CompletedProcess([], status, stdout, "synthetic diagnostic")
            with mock.patch.object(runner, "SOURCE", source), mock.patch.dict(runner.os.environ, {"FIN_ANVIL_PATH": str(anvil)}), mock.patch.object(runner.shutil, "which", return_value="synthetic-tool"), mock.patch.object(runner.subprocess, "run", return_value=completed) as command, mock.patch.object(runner, "start_owned_process") as start:
                self.assertEqual(runner.verify(), 1)
                start.assert_not_called()
                self.assertEqual(command.call_count, 1)
                self.assertIn("deployed_base_aave_revision_and_provider_readiness", command.call_args[0][0])
                report = json.loads(next((source / "work").glob("*/pin.json")).read_text())
                self.assertFalse(report["live_writes"])
                self.assertIn("failure", report)
                self.assertIn("fin_gap", report)

    def test_readiness_failure_never_starts_fork(self):
        self.run_failure(1, "synthetic required read failed\n")

    def test_multiple_pins_cannot_choose_a_new_fork_identity(self):
        pin = {"chain_id": 8453, "block": 1, "hash": "0x" + "ab" * 32, "timestamp": 1}
        self.run_failure(0, ("BASE_AAVE_PIN=" + json.dumps(pin) + "\n") * 2)

    def test_wrong_chain_pin_never_starts_fork(self):
        pin = {"chain_id": 1, "block": 1, "hash": "0x" + "ab" * 32, "timestamp": 1}
        self.run_failure(0, "BASE_AAVE_PIN=" + json.dumps(pin) + "\n")


if __name__ == "__main__":
    unittest.main()
