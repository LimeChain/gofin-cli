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


def ci_profile(registered, required, document):
    """Validate the entire checked-in profile, even for a single group run."""
    if not isinstance(document, dict) or set(document) != set(required):
        raise ValueError("CI profile must contain exactly the required scenario groups")
    selected = set()
    for group, names in document.items():
        if not isinstance(names, list) or not names:
            raise ValueError(f"CI profile group {group} must be nonempty")
        for name in names:
            if not isinstance(name, str) or name not in registered:
                raise ValueError(f"CI profile references an unknown scenario: {name}")
            if name in selected:
                raise ValueError(f"CI profile contains a duplicate scenario: {name}")
            if registered[name]["group"] != group:
                raise ValueError(f"CI profile scenario {name} belongs to a different group")
            selected.add(name)
    return selected


def unique_json_object(pairs):
    value = {}
    for key, item in pairs:
        if key in value:
            raise ValueError(f"Duplicate CI profile key: {key}")
        value[key] = item
    return value


def expand(value, substitutions):
    for key, replacement in substitutions.items():
        value = value.replace("{" + key + "}", str(replacement))
    return value


def read_execution_evidence(path, scenario_id):
    evidence = json.loads(path.read_text())
    if evidence.get("scenario") != scenario_id:
        raise ValueError("Execution evidence belongs to a different scenario")
    for key in ("approval_count", "external_submission_count", "intervention_count"):
        if type(evidence.get(key)) is not int or evidence[key] < 0:
            raise ValueError(f"Execution evidence needs a nonnegative {key}")
    if evidence["intervention_count"]:
        raise ValueError("Recovery required an intervention")
    if type(evidence.get("remaining_external_uncertainty")) is not bool:
        raise ValueError("Execution evidence must report remaining external uncertainty")
    if "blocked_unrelated_activity" not in evidence:
        raise ValueError("Execution evidence must report the unrelated-admission probe")
    if re.fullmatch(r"outcome\.a[0-9]+", scenario_id) and evidence["blocked_unrelated_activity"] is not False:
        raise ValueError("Acceptance evidence requires successful same-account and different-account admission probes")
    if re.fullmatch(r"outcome\.a[0-9]+", scenario_id):
        probes = execution_probes(evidence)
        for scope in ("same_account", "different_account"):
            if not any(probe["scope"] == scope for probe in probes):
                raise ValueError(f"Acceptance evidence has no observed {scope} admission")
    return evidence


def execution_probes(evidence):
    probes = evidence.get("admission_probes", [])
    if not isinstance(probes, list):
        raise ValueError("Admission probes must be an array")
    for probe in probes:
        if not isinstance(probe, dict) or probe.get("scope") not in ("same_account", "different_account"):
            raise ValueError("Admission probe needs an explicit account scope")
        if probe.get("admitted") is not True or not isinstance(probe.get("operation_id"), str) or not probe["operation_id"]:
            raise ValueError("Admission probe must identify an admitted logical action")
        if not isinstance(probe.get("submission"), dict) or not probe["submission"]:
            raise ValueError("Admission probe must retain its durable submission identity")
        for field in ("approval_count", "external_submission_count"):
            if type(probe.get(field)) is not int or probe[field] < (1 if field == "external_submission_count" else 0):
                raise ValueError(f"Admission probe needs observed {field}")
    result = list(probes)
    for component in evidence.get("components", []):
        result.extend(execution_probes(component))
    return result


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
    test_argv = argv
    if Path(test_argv[0]).name == "env":
        test_argv = test_argv[1:]
        while test_argv and "=" in test_argv[0]:
            test_argv = test_argv[1:]
    if process.returncode:
        result["error"] = f"Command exited {process.returncode}"
    elif test_argv and "cargo" in Path(test_argv[0]).name and "test" in test_argv and "--no-run" not in test_argv:
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
    parser.add_argument("--profile", choices=["full", "ci"], default="full")
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
    profile_ids = set(registered)
    if args.profile == "ci":
        try:
            profile_ids = ci_profile(registered, required, json.loads(
                (MANIFESTS / "ci.json").read_text(), object_pairs_hook=unique_json_object))
        except (OSError, ValueError, TypeError) as error:
            parser.error(f"Invalid CI profile: {error}")
        outside = set(args.scenario) - profile_ids
        if outside:
            parser.error(f"Scenarios outside CI profile: {sorted(outside)}")
    selected = [s for s in registered.values() if
                s["id"] in profile_ids and
                (not args.scenario or s["id"] in args.scenario) and
                (not args.group or s["group"] in args.group)]
    filtered = set(args.scenario) - {s["id"] for s in selected}
    if filtered:
        parser.error(f"Scenarios do not belong to the selected groups: {sorted(filtered)}")
    needed = {name for group, names in required.items() if not args.group or group in args.group for name in names}
    missing = sorted(needed - registered.keys()) if args.profile == "full" and not args.scenario else []
    if args.list:
        for scenario in selected:
            print(f"{scenario['id']}\t{scenario['group']}\t{scenario.get('environment', 'mock')}\t{scenario['description']}")
        for name in missing:
            print(f"{name}\tNOT_IMPLEMENTED\tRequired scenario has no executable registration")
        return 1 if missing else 0
    if not selected and not missing:
        parser.error("No scenarios selected")
    stamp = dt.datetime.now(dt.timezone.utc).strftime("%Y%m%dT%H%M%S.%fZ")
    artifacts = ROOT / ".context" / "test-runs" / f"positive-e2e-{stamp}"
    artifacts.mkdir(parents=True)
    report_path = args.report.resolve() if args.report else artifacts / "report.json"
    report_path.parent.mkdir(parents=True, exist_ok=True)
    report = {"schema_version": 1, "started_at": stamp, "profile": args.profile,
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
                        # Keep the diagnostic visible even if artifact upload fails.
                        if logfile.is_file():
                            tail = logfile.read_text(errors="replace").splitlines()[-60:]
                            print("\n".join(tail), flush=True)
                        break
                else:
                    row["status"] = "passed"
                    if scenario.get("evidence"):
                        try:
                            evidence_path = Path(expand(scenario["evidence"], substitutions))
                            row["execution"] = read_execution_evidence(evidence_path, name)
                            row["evidence"] = str(evidence_path)
                        except (OSError, ValueError, TypeError) as error:
                            row.update(status="failed", error=f"Invalid execution evidence: {error}")
                    print(f"{row['status'].upper()} {name}", flush=True)
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
