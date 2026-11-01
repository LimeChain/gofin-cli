#!/usr/bin/env python3
"""Validate positive E2E profile selection and fail-closed behavior."""
import contextlib
import importlib.util
import io
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

ROOT = Path(__file__).resolve().parents[1]
RUNNER = ROOT / "scripts/run-e2e.py"
spec = importlib.util.spec_from_file_location("run_e2e", RUNNER)
runner = importlib.util.module_from_spec(spec)
spec.loader.exec_module(runner)


class Profiles(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.registered = runner.scenarios()
        cls.required = json.loads((runner.MANIFESTS / "required.json").read_text())
        cls.ci = json.loads((runner.MANIFESTS / "ci.json").read_text())

    def listed(self, *args):
        result = subprocess.run([sys.executable, str(RUNNER), "--list", *args],
                                cwd=ROOT, text=True, capture_output=True)
        return result, {line.split("\t")[0] for line in result.stdout.splitlines()}

    def test_default_is_full_and_ci_is_exact_subset(self):
        full, full_ids = self.listed()
        ci, ci_ids = self.listed("--profile", "ci")
        self.assertEqual(full.returncode, 0, full.stderr)
        self.assertEqual(ci.returncode, 0, ci.stderr)
        self.assertEqual(full_ids, set(self.registered))
        self.assertEqual(ci_ids, {name for names in self.ci.values() for name in names})
        self.assertLess(ci_ids, full_ids)
        self.assertTrue({name for names in self.required.values() for name in names} <= full_ids)
        self.assertEqual({group: len(names) for group, names in self.ci.items()},
                         {"core": 2, "exchange": 10, "evm": 6, "bridge": 9,
                          "solana": 3, "papertrade": 2, "outcome": 27})

    def test_group_and_explicit_selection(self):
        for group, names in self.ci.items():
            result, ids = self.listed("--profile", "ci", "--group", group)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(ids, set(names))
        selected = self.ci["evm"][0]
        result, ids = self.listed("--profile", "ci", "--scenario", selected)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(ids, {selected})
        outside = next(iter(set(self.registered) - {x for a in self.ci.values() for x in a}))
        result, _ = self.listed("--profile", "ci", "--scenario", outside)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("outside CI profile", result.stderr)
        result, _ = self.listed("--profile", "full", "--scenario", outside)
        self.assertEqual(result.returncode, 0, result.stderr)
        result, _ = self.listed("--profile", "ci", "--group", "core", "--scenario", selected)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("selected groups", result.stderr)

    def test_malformed_or_incomplete_profile_fails_closed(self):
        valid = {"a": {"id": "a", "group": "one"}, "b": {"id": "b", "group": "two"}}
        required = {"one": ["a"], "two": ["b"]}
        for bad in [None, {}, {"one": ["a"]}, {"one": [], "two": ["b"]},
                    {"one": ["a", "a"], "two": ["b"]},
                    {"one": ["b"], "two": ["a"]},
                    {"one": ["missing"], "two": ["b"]},
                    {"one": "a", "two": ["b"]},
                    {"one": ["a"], "two": ["b"], "extra": ["a"]}]:
            with self.subTest(bad=bad), self.assertRaises(ValueError):
                runner.ci_profile(valid, required, bad)
        self.assertEqual(runner.ci_profile(valid, required, {"one": ["a"], "two": ["b"]}), {"a", "b"})
        with self.assertRaises(ValueError):
            json.loads('{"one":["a"],"one":["a"]}', object_pairs_hook=runner.unique_json_object)

    def test_full_missing_required_definition_fails_list(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory)
            (path / "required.json").write_text(json.dumps({"one": ["missing"]}))
            with mock.patch.object(runner, "MANIFESTS", path), mock.patch.object(runner, "scenarios", return_value={"a": {"id": "a", "group": "one", "description": "a", "run": [["true"]]}}), mock.patch.object(sys, "argv", ["run-e2e", "--list"]), contextlib.redirect_stdout(io.StringIO()) as output:
                self.assertEqual(runner.main(), 1)
                self.assertIn("missing\tNOT_IMPLEMENTED", output.getvalue())

    def test_report_records_selected_profile(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "required.json").write_text(json.dumps({"one": ["a"]}))
            (root / "ci.json").write_text(json.dumps({"one": ["a"]}))
            report = root / "report.json"
            registered = {"a": {"id": "a", "group": "one", "description": "a", "run": [["true"]]}}
            for profile in ("full", "ci"):
                with mock.patch.object(runner, "ROOT", root), mock.patch.object(runner, "MANIFESTS", root), mock.patch.object(runner, "scenarios", return_value=registered), mock.patch.object(runner, "source_snapshot", return_value={}), mock.patch.object(runner, "run_command", return_value={"exit_code": 0}), mock.patch.object(sys, "argv", ["run-e2e", "--profile", profile, "--report", str(report)]), contextlib.redirect_stdout(io.StringIO()):
                    self.assertEqual(runner.main(), 0)
                self.assertEqual(json.loads(report.read_text())["profile"], profile)


if __name__ == "__main__":
    unittest.main()
