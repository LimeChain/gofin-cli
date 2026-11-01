#!/usr/bin/env python3
"""Reject incomplete or misleading execution evidence in the release gate."""
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("e2e_runner", Path(__file__).with_name("run-e2e.py"))
runner = importlib.util.module_from_spec(spec)
spec.loader.exec_module(runner)

aggregate_spec = importlib.util.spec_from_file_location("aggregate_evidence", Path(__file__).parent / "e2e" / "aggregate-outcome-evidence.py")
aggregator = importlib.util.module_from_spec(aggregate_spec)
aggregate_spec.loader.exec_module(aggregator)


class OutcomeEvidenceTests(unittest.TestCase):
    def check(self, **changes):
        evidence = dict(scenario="outcome.a2", approval_count=1, external_submission_count=1,
                        intervention_count=0, remaining_external_uncertainty=False,
                        blocked_unrelated_activity=False, admission_probes=[
                            dict(scope=scope, operation_id=scope, submission={"reference":scope},
                                 approval_count=1, external_submission_count=1, admitted=True)
                            for scope in ("same_account", "different_account")])
        evidence.update(changes)
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "evidence.json"
            path.write_text(json.dumps(evidence))
            return runner.read_execution_evidence(path, "outcome.a2")

    def test_environment_wrapped_cargo_cannot_hide_skipped_tests(self):
        def completed(argv, **kwargs):
            kwargs["stdout"].write("test result: ok. 0 passed; 0 failed; 1 ignored;\n")
            return type("Completed", (), {"returncode": 0})()
        with tempfile.TemporaryDirectory() as directory, \
                patch.object(runner, "source_snapshot", return_value={}), \
                patch.object(runner.subprocess, "run", side_effect=completed):
            result = runner.run_command({"argv": ["env", "FIXTURE=synthetic", "cargo", "test"],
                                         "expected_tests": 1}, {}, {}, Path(directory) / "test.log")
            self.assertIn("skips are not passes", result["error"])

    def test_exact_observed_counts_and_probes_are_accepted(self):
        self.assertEqual(self.check()["external_submission_count"], 1)
        self.assertTrue(self.check(remaining_external_uncertainty=True)["remaining_external_uncertainty"])

    def test_foreign_evidence_or_recovery_interventions_cannot_pass(self):
        for change in [dict(scenario="outcome.a3"), dict(intervention_count=1)]:
            with self.subTest(change=change), self.assertRaises(ValueError):
                self.check(**change)

    def test_counts_cannot_be_missing_negative_or_boolean(self):
        for field in ["approval_count", "external_submission_count", "intervention_count"]:
            for value in [None, -1, True, "1"]:
                with self.subTest(field=field, value=value), self.assertRaises(ValueError):
                    self.check(**{field: value})

    def test_unrun_or_blocked_probes_cannot_pass_acceptance(self):
        for value in [None, True, 0, "false"]:
            with self.subTest(value=value), self.assertRaises(ValueError):
                self.check(blocked_unrelated_activity=value)

    def test_boolean_claim_cannot_replace_observed_admissions(self):
        for probes in [[], None, [{"scope":"same_account","admitted":True}],
                       [dict(scope="same_account", operation_id="same", submission={"reference":"hash"},
                             approval_count=1, external_submission_count=1, admitted=True)]]:
            with self.subTest(probes=probes), self.assertRaises(ValueError):
                self.check(admission_probes=probes)

    def test_aggregation_preserves_counts_uncertainty_and_rejects_missing_evidence(self):
        with tempfile.TemporaryDirectory() as directory:
            paths = [Path(directory) / name for name in ("a.json", "b.json")]
            evidence = dict(scenario="venue.a", approval_count=2, external_submission_count=3,
                            intervention_count=0, remaining_external_uncertainty=False,
                            blocked_unrelated_activity=False)
            paths[0].write_text(json.dumps(evidence))
            evidence.update(scenario="venue.b", remaining_external_uncertainty=True)
            paths[1].write_text(json.dumps(evidence))
            result = aggregator.aggregate("outcome.a19", paths)
            self.assertEqual(result["approval_count"], 4)
            self.assertEqual(result["external_submission_count"], 6)
            self.assertTrue(result["remaining_external_uncertainty"])
            with self.assertRaises(AssertionError):
                aggregator.aggregate("outcome.a19", [paths[0], paths[0]])
            del evidence["approval_count"]
            paths[1].write_text(json.dumps(evidence))
            with self.assertRaises(KeyError):
                aggregator.aggregate("outcome.a19", paths)


if __name__ == "__main__":
    unittest.main()
