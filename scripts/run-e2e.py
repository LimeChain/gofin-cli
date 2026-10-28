#!/usr/bin/env python3
"""Run the explicit positive CLI acceptance scenarios, with durable evidence."""
from __future__ import annotations

import argparse
import datetime as dt
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import time

ROOT = Path(__file__).resolve().parents[1]
MANIFESTS = ROOT / "scripts" / "e2e"


def scenarios():
    result = {}
    for path in sorted(MANIFESTS.glob("*-scenarios.json")):
        document = json.loads(path.read_text())
        rows = document["scenarios"] if isinstance(document, dict) else document
        for row in rows:
            name = row["id"]
            if not re.fullmatch(r"[a-z0-9][a-z0-9._-]*", name) or name in result:
                raise ValueError(f"Invalid or duplicate scenario ID: {name}")
            if not row.get("run") or not row.get("description") or not row.get("group"):
                raise ValueError(f"Incomplete scenario: {name}")
            for command in row.get("prepare", []) + row["run"]:
                argv = command.get("argv") if isinstance(command, dict) else command
                if not isinstance(argv, list) or not argv or not all(isinstance(a, str) for a in argv):
                    raise ValueError(f"Commands must be argument arrays: {name}")
            result[name] = row
    if not result:
        raise ValueError("No positive E2E scenarios registered")
    return result


def expand(value, substitutions):
    for key, replacement in substitutions.items():
        value = value.replace("{" + key + "}", str(replacement))
    return value


def source_snapshot():
    diff = subprocess.check_output(["git", "diff", "HEAD", "--binary"], cwd=ROOT)
    untracked = subprocess.check_output(
        ["git", "ls-files", "--others", "--exclude-standard", "-z"], cwd=ROOT
    ).decode().split("\0")
    return {
        "commit": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip(),
        "tracked_diff_sha256": hashlib.sha256(diff).hexdigest(),
        "untracked_file_sha256": {
            name: hashlib.sha256((ROOT / name).read_bytes()).hexdigest()
            for name in sorted(filter(None, untracked)) if (ROOT / name).is_file()
        },
    }


def run_command(command, env, substitutions, logfile, default_expected=None):
    specification = command if isinstance(command, dict) else {"argv": command}
    argv = [expand(value, substitutions) for value in specification["argv"]]
    expected = specification.get("expected_tests", default_expected)
    before = source_snapshot()
    started = time.monotonic()
    with logfile.open("w") as output:
        process = subprocess.run(argv, cwd=ROOT, env=env, stdout=output, stderr=subprocess.STDOUT)
    result = {"argv": argv, "exit_code": process.returncode,
              "elapsed_seconds": round(time.monotonic() - started, 3), "log": str(logfile)}
    after = source_snapshot()
    result.update(source_before=before, source_after=after, source_changed=before != after)
    if process.returncode:
        result["error"] = f"Command exited {process.returncode}"
    elif "cargo" in Path(argv[0]).name and "test" in argv and "--no-run" not in argv:
        log = logfile.read_text(errors="replace")
        passes = sum(int(n) for n in re.findall(r"test result: ok\. (\d+) passed;", log))
        ignored = sum(int(n) for n in re.findall(r"test result: ok\. \d+ passed; \d+ failed; (\d+) ignored;", log))
        result.update(passed_tests=passes, ignored_tests=ignored)
        if passes == 0 or ignored:
            result["error"] = f"Acceptance command ran {passes} tests and ignored {ignored}; skips are not passes"
        elif expected is not None and passes != expected:
            result["error"] = f"Expected {expected} tests, observed {passes}"
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--list", action="store_true")
    parser.add_argument("--group", action="append", default=[])
    parser.add_argument("--scenario", action="append", default=[])
    parser.add_argument("--report", type=Path)
    parser.add_argument("--fail-fast", action="store_true")
    parser.add_argument("--skip-prepare", action="store_true", help="Use already-built binaries (for controlled mutation verification)")
    args = parser.parse_args()
    registered = scenarios()
    required = json.loads((MANIFESTS / "required.json").read_text())
    for group, names in required.items():
        for name in names:
            if name in registered and registered[name]["group"] != group:
                parser.error(f"Required scenario {name} belongs to {group}")
    unknown = set(args.scenario) - registered.keys()
    unknown_groups = set(args.group) - required.keys()
    if unknown or unknown_groups:
        parser.error(f"Unknown scenarios/groups: {sorted(unknown | unknown_groups)}")
    selected = [s for s in registered.values() if
                (not args.scenario or s["id"] in args.scenario) and
                (not args.group or s["group"] in args.group)]
    needed = {name for group, names in required.items() if not args.group or group in args.group for name in names}
    missing = sorted(needed - registered.keys()) if not args.scenario else []
    if args.list:
        for scenario in selected:
            print(f"{scenario['id']}\t{scenario['group']}\t{scenario.get('environment', 'mock')}\t{scenario['description']}")
        for name in missing:
            print(f"{name}\tNOT_IMPLEMENTED\tRequired scenario has no executable registration")
        return 0
    if not selected and not missing:
        parser.error("No scenarios selected")
    stamp = dt.datetime.now(dt.timezone.utc).strftime("%Y%m%dT%H%M%S.%fZ")
    artifacts = ROOT / ".context" / "test-runs" / f"positive-e2e-{stamp}"
    artifacts.mkdir(parents=True)
    report_path = args.report.resolve() if args.report else artifacts / "report.json"
    report_path.parent.mkdir(parents=True, exist_ok=True)
    report = {"schema_version": 1, "started_at": stamp,
              **source_snapshot(),
              "prepare_skipped": args.skip_prepare,
              "scenarios": [{"id": name, "status": "not_implemented"} for name in missing]}
    for name in missing:
        print(f"FAIL {name}: required scenario is not implemented", flush=True)
    report_path.write_text(json.dumps(report, indent=2) + "\n")
    substitutions = {"repo": ROOT, "python": sys.executable, "artifacts": artifacts}
    try:
        for scenario in selected:
            name = scenario["id"]
            row = {"id": name, "environment": scenario.get("environment", "mock"),
                   "status": "running", "commands": []}
            report["scenarios"].append(row)
            report_path.write_text(json.dumps(report, indent=2) + "\n")
            env = dict(os.environ)
            env.update({key: expand(value, substitutions) for key, value in scenario.get("env", {}).items()})
            missing = [tool for tool in scenario.get("requires", []) if not shutil.which(tool, path=env.get("PATH"))]
            print(f"RUN {name}", flush=True)
            if missing:
                row.update(status="failed", error=f"Missing prerequisites: {', '.join(missing)}")
            else:
                commands = ([] if args.skip_prepare else scenario.get("prepare", [])) + scenario["run"]
                for index, command in enumerate(commands):
                    logfile = artifacts / f"{name}.{index + 1}.log"
                    try:
                        outcome = run_command(command, env, substitutions, logfile, scenario.get("expected_tests"))
                    except OSError as error:
                        outcome = {"error": str(error), "log": str(logfile)}
                    row["commands"].append(outcome)
                    if "error" in outcome:
                        row.update(status="failed", error=outcome["error"])
                        print(f"FAIL {name}: {row['error']} ({logfile})", flush=True)
                        break
                else:
                    row["status"] = "passed"
                    print(f"PASS {name}", flush=True)
            report_path.write_text(json.dumps(report, indent=2) + "\n")
            if row["status"] == "failed" and args.fail_fast:
                break
    except KeyboardInterrupt:
        if report["scenarios"] and report["scenarios"][-1]["status"] == "running":
            report["scenarios"][-1]["status"] = "interrupted"
    executed = {row["id"] for row in report["scenarios"]}
    report["scenarios"].extend({"id": s["id"], "status": "not_run"} for s in selected if s["id"] not in executed)
    report["status"] = "passed" if all(r["status"] == "passed" for r in report["scenarios"]) else "failed"
    report_path.write_text(json.dumps(report, indent=2) + "\n")
    print(f"Report: {report_path}", flush=True)
    return 0 if report["status"] == "passed" else 1


if __name__ == "__main__":
    sys.exit(main())
