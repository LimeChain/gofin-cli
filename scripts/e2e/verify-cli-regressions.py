#!/usr/bin/env python3
"""Verify source-level CLI regressions are caught, then restore and recheck.

Run without concurrent workspace builds: this deliberately changes the CLI source
temporarily. Its original bytes are restored in finally, including on Ctrl-C.
"""
import argparse
import datetime as dt
import hashlib
import json
from pathlib import Path
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[2]
SOURCE = ROOT / "crates/gofin-cli/src/main.rs"
MUTATIONS = {
    "discovery-requires-broker": (
        '"operations": financial_catalog(),\n        "service_required": false,',
        '"operations": financial_catalog(),\n        "service_required": true,',
        'assert caps["service_required"] is False'),
    "schema-returns-output-for-input": (
        'fn schema(name: &str) -> Option<Value> {\n    let definition = find_definition(name)?;\n    Some(json!({\n        "name":name,\n        "input":definition.get("input_schema"),',
        'fn schema(name: &str) -> Option<Value> {\n    let definition = find_definition(name)?;\n    Some(json!({\n        "name":name,\n        "input":definition.get("output_schema"),',
        'assert schema[key] == definition[f"{key}_schema"]'),
    "execute-reuses-request-id": (
        '.execute(OperationRequest {\n                        request_id: args.id.ok_or_else(generic_input_error)?,\n                        operation: args.operation,',
        '.execute(OperationRequest {\n                        request_id: "mutation-reused-id".into(),\n                        operation: args.operation,',
        'REQUEST_ID_CONFLICT'),
}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--mutation", choices=list(MUTATIONS), action="append")
    args = parser.parse_args()
    original = SOURCE.read_bytes()
    stamp = dt.datetime.now(dt.timezone.utc).strftime("%Y%m%dT%H%M%S.%fZ")
    directory = ROOT / ".context/test-runs" / f"cli-regressions-{stamp}"
    directory.mkdir(parents=True)
    (directory / "original-main.rs").write_bytes(original)
    report = {"source": str(SOURCE), "original_sha256": hashlib.sha256(original).hexdigest(), "mutations": []}

    def command(argv, log):
        with (directory / log).open("w") as output:
            return subprocess.run(argv, cwd=ROOT, stdout=output, stderr=subprocess.STDOUT).returncode

    def build(log):
        assert command(["cargo", "build", "--locked", "-p", "gofin-cli", "--bin", "gofin"], log) == 0, f"Build failed: {log}"

    def scenario(name, log):
        if name == "execute-reuses-request-id":
            return command([sys.executable, "scripts/run-e2e.py", "--scenario", "coinbase-exchange-orders",
                            "--report", str(directory / (log + ".json"))], log)
        return command([sys.executable, "scripts/e2e/discovery.py", "--cli", "target/debug/gofin"], log)

    def evidence(log):
        output = (directory / log).read_text()
        nested_report = directory / (log + ".json")
        if nested_report.exists():
            for case in json.loads(nested_report.read_text())["scenarios"]:
                for invocation in case.get("commands", []):
                    if "log" in invocation:
                        output += Path(invocation["log"]).read_text()
        return output

    try:
        for name in args.mutation or MUTATIONS:
            build(f"{name}.baseline-build.log")
            assert scenario(name, f"{name}.baseline.log") == 0, "Baseline must pass before mutation"
            before, after, assertion = MUTATIONS[name]
            text = original.decode()
            assert text.count(before) == 1, f"Mutation anchor is not unique: {name}"
            mutated = text.replace(before, after, 1).encode()
            row = {"id": name, "mutated_sha256": hashlib.sha256(mutated).hexdigest()}
            report["mutations"].append(row)
            SOURCE.write_bytes(mutated)
            try:
                build(f"{name}.build.log")
                result = scenario(name, f"{name}.failed.log")
                assert result != 0 and assertion in evidence(f"{name}.failed.log"), f"Mutation not caught by expected assertion: {name}"
                row["regression_detected"] = True
            finally:
                SOURCE.write_bytes(original)
                build(f"{name}.restored-build.log")
                assert scenario(name, f"{name}.restored.log") == 0, f"Restored CLI did not pass: {name}"
                row["restored_passed"] = True
            print(f"PASS {name}: baseline passed, source regression failed, restored source passed", flush=True)
        report["status"] = "passed"
    finally:
        SOURCE.write_bytes(original)
        report["source_restored"] = SOURCE.read_bytes() == original
        report.setdefault("status", "incomplete")
        (directory / "report.json").write_text(json.dumps(report, indent=2) + "\n")
        print(f"Evidence: {directory}", flush=True)


if __name__ == "__main__":
    main()
