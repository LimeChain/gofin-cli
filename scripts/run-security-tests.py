#!/usr/bin/env python3
"""Run the explicitly retained security-negative Rust regressions."""
from __future__ import annotations

import argparse
import datetime as dt
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import time

ROOT = Path(__file__).resolve().parents[1]


def selections():
    rows = []
    identities = set()
    for path in sorted((ROOT / "scripts/security").glob("*.json")):
        if path.name == "required.json":
            continue
        for row in json.loads(path.read_text()):
            key = row["id"]
            if key in identities or not row["tests"] or not row["reason"]:
                raise ValueError(f"Duplicate or incomplete security group: {key}")
            if len(row["tests"]) != len(set(row["tests"])):
                raise ValueError(f"Duplicate security tests: {key}")
            if row["target"][0] not in ("--lib", "--bin", "--test"):
                raise ValueError(f"Invalid Rust target: {key}")
            identities.add(key)
            rows.append(row)
    validate_coverage(rows)
    return rows


def validate_coverage(rows):
    required = json.loads((ROOT / "scripts/security/required.json").read_text())
    observed = {r["id"]: len(r["tests"]) for r in rows}
    if not required or observed != required:
        raise ValueError(f"Security coverage differs from required groups/counts: expected {required}, observed {observed}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--list", action="store_true")
    parser.add_argument("--group", action="append", default=[])
    args = parser.parse_args()
    rows = selections()
    unknown = set(args.group) - {r["id"] for r in rows}
    if unknown:
        parser.error(f"Unknown security groups: {sorted(unknown)}")
    rows = [r for r in rows if not args.group or r["id"] in args.group]
    if args.list:
        for row in rows:
            print(f"{row['id']}\t{len(row['tests'])} tests\t{row['reason']}")
        return 0
    stamp = dt.datetime.now(dt.timezone.utc).strftime("%Y%m%dT%H%M%S.%fZ")
    output = ROOT / ".context/test-runs" / f"security-{stamp}"
    output.mkdir(parents=True)
    manifest = json.dumps(rows, sort_keys=True).encode()
    report = {"started_at": stamp, "manifest_sha256": hashlib.sha256(manifest).hexdigest(),
              "commit": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip(),
              "tracked_diff_sha256": hashlib.sha256(subprocess.check_output(["git", "diff", "HEAD", "--binary"], cwd=ROOT)).hexdigest(),
              "groups": []}
    report_path = output / "report.json"
    try:
        for row in rows:
            entry = {"id": row["id"], "status": "running", "commands": []}
            report["groups"].append(entry)
            report_path.write_text(json.dumps(report, indent=2) + "\n")
            print(f"RUN {row['id']}", flush=True)
            base = ["cargo", "test", "--locked", "-p", row["package"], *row["target"]]

            def run(argv, suffix):
                log = output / f"{row['id']}.{suffix}.log"
                started = time.monotonic()
                with log.open("w") as stream:
                    completed = subprocess.run(argv, cwd=ROOT, env=os.environ, stdout=stream, stderr=subprocess.STDOUT)
                entry["commands"].append({"argv": argv, "exit_code": completed.returncode,
                                          "elapsed_seconds": round(time.monotonic() - started, 3), "log": str(log)})
                if completed.returncode:
                    raise RuntimeError(f"Command exited {completed.returncode}; see {log}")
                return log.read_text(errors="replace")

            try:
                listing = run(base + ["--", "--list"], "list")
                available = re.findall(r"^([\w:]+): test$", listing, re.MULTILINE)
                exact = []
                for requested in row["tests"]:
                    matches = [name for name in available if name == requested or ("::" not in requested and name.rsplit("::", 1)[-1] == requested)]
                    if len(matches) != 1:
                        raise RuntimeError(f"{requested}: expected one exact test identity, found {matches}")
                    exact.append(matches[0])
                if len(exact) != len(set(exact)):
                    raise RuntimeError("Multiple selectors resolve to the same test")
                log = run(base + ["--", "--exact", *exact], "run")
                passed = sum(map(int, re.findall(r"test result: ok\. (\d+) passed;", log)))
                ignored = sum(map(int, re.findall(r"test result: ok\. \d+ passed; \d+ failed; (\d+) ignored;", log)))
                if passed != len(exact) or ignored:
                    raise RuntimeError(f"Expected {len(exact)} passed, observed {passed} passed and {ignored} ignored")
                entry.update(status="passed", tests=exact, passed_tests=passed)
                print(f"PASS {row['id']}: {passed}", flush=True)
            except (OSError, RuntimeError) as error:
                entry.update(status="failed", error=str(error))
                print(f"FAIL {row['id']}: {error}", flush=True)
            report_path.write_text(json.dumps(report, indent=2) + "\n")
    except KeyboardInterrupt:
        if report["groups"] and report["groups"][-1]["status"] == "running":
            report["groups"][-1]["status"] = "interrupted"
    executed = {r["id"] for r in report["groups"]}
    report["groups"].extend({"id": r["id"], "status": "not_run"} for r in rows if r["id"] not in executed)
    report["status"] = "passed" if all(r["status"] == "passed" for r in report["groups"]) else "failed"
    report_path.write_text(json.dumps(report, indent=2) + "\n")
    print(f"Report: {report_path}", flush=True)
    return 0 if report["status"] == "passed" else 1


if __name__ == "__main__":
    sys.exit(main())
