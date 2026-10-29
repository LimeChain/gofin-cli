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
    if len(observed) != len(rows):
        raise ValueError("Duplicate security group identities")
    if not required or observed != required:
        raise ValueError(f"Security coverage differs from required groups/counts: expected {required}, observed {observed}")


def parse_target(row):
    """Keep target/feature selection explicit; reject flags that could broaden coverage."""
    args = row["target"]
    if not isinstance(args, list) or not args:
        raise ValueError("A security group must select a Rust target")
    kind = args[0]
    if kind == "--lib":
        name, rest = None, args[1:]
    elif kind in ("--bin", "--test") and len(args) >= 2 and isinstance(args[1], str) and args[1] and not args[1].startswith("-"):
        name, rest = args[1], args[2:]
    else:
        raise ValueError(f"Invalid Rust target: {args}")
    features = ()
    if rest:
        if len(rest) != 2 or rest[0] != "--features" or not isinstance(rest[1], str):
            raise ValueError(f"Unsupported security target flags: {rest}")
        features = tuple(sorted(rest[1].split(",")))
        if len(set(features)) != len(features) or any(not re.fullmatch(r"[A-Za-z0-9_-]+", f) for f in features):
            raise ValueError(f"Invalid security features: {rest[1]}")
    return kind, name, features


def parse_feature_graph(text):
    """Retain separate host/target feature variants instead of flattening them."""
    graph = {}
    for line in text.splitlines():
        if not line.strip():
            continue
        if line.endswith(" (*)"):
            line = line[:-4]
        if line.count("|") != 1:
            raise ValueError(f"Malformed Cargo feature graph row: {line}")
        identity, feature_text = line.split("|")
        if not re.fullmatch(r"\S+ v\S+(?: .+)?", identity):
            raise ValueError(f"Malformed Cargo package identity: {identity}")
        features = frozenset(feature_text.split(",")) if feature_text else frozenset()
        if any(not re.fullmatch(r"[A-Za-z0-9_+.-]+", f) for f in features):
            raise ValueError(f"Malformed Cargo feature set: {feature_text}")
        graph.setdefault(identity, set()).add(features)
    if not graph:
        raise ValueError("Cargo returned an empty feature graph")
    return graph


def graphs_compatible(left, right):
    return all(left[key] == right[key] for key in left.keys() & right.keys())


def merge_feature_graphs(graphs):
    merged = {}
    for graph in graphs:
        if not graphs_compatible(merged, graph):
            raise ValueError("Incompatible feature graphs cannot share a build")
        merged.update(graph)
    return merged


def feature_graph_json(graph):
    return {key: sorted(sorted(features) for features in variants) for key, variants in sorted(graph.items())}


def package_arguments(contexts):
    argv = []
    for package in sorted({package for package, _ in contexts}):
        argv.extend(["-p", package])
    features = sorted({f"{package}/{feature}" for package, selected in contexts for feature in selected})
    if features:
        argv.extend(["--features", ",".join(features)])
    return argv


def tree_command(contexts):
    return ["cargo", "tree", "--locked", *package_arguments(contexts), "-e", "normal,build,dev",
            "--prefix", "none", "--format", "{p}|{f}", "--color", "never"]


def build_command(rows):
    contexts = {(row["package"], parse_target(row)[2]) for row in rows}
    argv = ["cargo", "test", "--locked", "--no-run", "--message-format=json", *package_arguments(contexts)]
    targets = sorted({parse_target(row)[:2] for row in rows}, key=lambda item: (item[0], item[1] or ""))
    for kind, name in targets:
        argv.append(kind)
        if name is not None:
            argv.append(name)
    return argv


def match_artifacts(messages, requests, packages):
    """Accept only Cargo's unique executable artifact for each selected test target."""
    expected = {}
    for row in requests:
        matches = [package for package in packages if package.get("name") == row["package"]]
        if len(matches) != 1:
            raise ValueError(f"Ambiguous or missing package metadata: {row['package']}")
        package = matches[0]
        kind, name, _ = parse_target(row)
        targets = [target for target in package["targets"]
                   if kind[2:] in target["kind"] and (name is None or target["name"] == name)]
        if len(targets) != 1:
            raise ValueError(f"Ambiguous or missing target metadata: {row['id']}")
        target = targets[0]
        expected[row["id"]] = (package, target)
    found = {}
    finished = []
    for message in messages:
        if not isinstance(message, dict) or not isinstance(message.get("reason"), str):
            raise ValueError("Malformed Cargo JSON message")
        if message["reason"] == "build-finished":
            finished.append(message.get("success"))
        if message["reason"] != "compiler-artifact":
            continue
        target = message.get("target")
        profile = message.get("profile")
        if not isinstance(target, dict) or not isinstance(profile, dict):
            raise ValueError("Malformed Cargo compiler artifact")
        if profile.get("test") is not True:
            continue
        for group, (package, selected) in expected.items():
            if message.get("package_id") != package["id"] or target.get("name") != selected["name"] or target.get("kind") != selected["kind"]:
                continue
            executable = message.get("executable")
            features = message.get("features")
            if (not isinstance(executable, str) or not Path(executable).is_absolute()
                    or not isinstance(features, list) or any(not isinstance(f, str) for f in features)):
                raise ValueError(f"Malformed selected test artifact: {group}")
            if group in found:
                raise ValueError(f"Ambiguous selected test artifact: {group}")
            found[group] = {"executable": executable, "cwd": str(Path(package["manifest_path"]).parent),
                            "package_id": package["id"], "target_name": selected["name"],
                            "target_kind": selected["kind"], "features": features}
    if finished != [True]:
        raise ValueError(f"Cargo did not emit one successful build-finished message: {finished}")
    missing = set(expected) - set(found)
    if missing:
        raise ValueError(f"Missing Cargo test executable artifacts: {sorted(missing)}")
    return found


def resolve_tests(requested, listing):
    available = re.findall(r"^([\w:]+): test$", listing, re.MULTILINE)
    if len(available) != len(set(available)):
        raise ValueError("Cargo test binary listed duplicate identities")
    exact = []
    for selector in requested:
        matches = [name for name in available if name == selector or ("::" not in selector and name.rsplit("::", 1)[-1] == selector)]
        if len(matches) != 1:
            raise ValueError(f"{selector}: expected one exact test identity, found {matches}")
        exact.append(matches[0])
    if len(exact) != len(set(exact)):
        raise ValueError("Multiple selectors resolve to the same test")
    return exact


def validate_results(log, exact):
    results = re.findall(r"^test ([\w:]+) \.\.\. (ok|FAILED|ignored(?:,.*)?)$", log, re.MULTILINE)
    if len(results) != len(exact) or {name for name, _ in results} != set(exact) or any(status != "ok" for _, status in results):
        raise ValueError(f"Test results differ from required exact identities: expected {exact}, observed {results}")
    summaries = re.findall(r"^test result: ok\. (\d+) passed; (\d+) failed; (\d+) ignored;", log, re.MULTILINE)
    if summaries != [(str(len(exact)), "0", "0")]:
        raise ValueError(f"Expected {len(exact)} passed and none failed/ignored, observed summaries {summaries}")
    return len(exact)


def build_library_paths(messages, target_directory):
    # Match Cargo's restriction to native build outputs within the target tree.
    # External native dependencies must already be on the caller's search path.
    paths = set()
    root = Path(target_directory).resolve()
    for message in messages:
        if message.get("reason") != "build-script-executed":
            continue
        linked = message.get("linked_paths", [])
        if not isinstance(linked, list) or any(not isinstance(path, str) for path in linked):
            raise ValueError("Malformed Cargo native library paths")
        for path in linked:
            _, separator, value = path.partition("=")
            candidate = Path(value if separator else path).resolve()
            if candidate.is_relative_to(root):
                paths.add(str(candidate))
    return sorted(paths)


def binary_environment(executable, target_directory, library_paths=(), rustlib=None):
    # Cargo normally adds these search paths before invoking a test executable.
    # Preserve the caller's paths as well, including custom native dependencies.
    if not Path(executable).resolve().is_relative_to(Path(target_directory).resolve()):
        raise ValueError("Cargo test executable is outside its target directory")
    env = os.environ.copy()
    search = [*library_paths, str(Path(executable).parent), str(Path(executable).parent.parent)]
    if rustlib:
        search.append(rustlib)
    if sys.platform == "darwin":
        key = "DYLD_FALLBACK_LIBRARY_PATH"
        fallback = [str(Path.home() / "lib"), "/usr/local/lib", "/usr/lib"]
    elif os.name == "nt":
        key, fallback = "PATH", []
    else:
        key, fallback = "LD_LIBRARY_PATH", []
    inherited = env.get(key)
    env[key] = os.pathsep.join(search + (inherited.split(os.pathsep) if inherited else fallback))
    # Compile-time CARGO_* values are already baked into each binary. Runtime
    # working directory is set separately to the package's manifest directory.
    return env


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--list", action="store_true")
    parser.add_argument("--group", action="append", default=[])
    args = parser.parse_args()
    rows = selections()
    for row in rows:
        parse_target(row)
    unknown = set(args.group) - {r["id"] for r in rows}
    if unknown:
        parser.error(f"Unknown security groups: {sorted(unknown)}")
    rows = [r for r in rows if not args.group or r["id"] in args.group]
    if args.list:
        for row in rows:
            print(f"{row['id']}\t{len(row['tests'])} tests\t{row['reason']}")
        return 0
    started = time.monotonic()
    stamp = dt.datetime.now(dt.timezone.utc).strftime("%Y%m%dT%H%M%S.%fZ")
    output = ROOT / ".context/test-runs" / f"security-{stamp}"
    output.mkdir(parents=True)
    manifest = json.dumps(rows, sort_keys=True).encode()
    report = {"started_at": stamp, "manifest_sha256": hashlib.sha256(manifest).hexdigest(),
              "commit": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip(),
              "tracked_diff_sha256": hashlib.sha256(subprocess.check_output(["git", "diff", "HEAD", "--binary"], cwd=ROOT)).hexdigest(),
              "preparation": [], "groups": [{"id": row["id"], "status": "not_run", "commands": []} for row in rows],
              "timings": {phase: 0.0 for phase in ("feature_resolution", "build", "listing", "execution")}}
    report_path = output / "report.json"

    def save():
        report_path.write_text(json.dumps(report, indent=2) + "\n")

    def run(argv, label, phase, records, *, cwd=ROOT, env=None):
        log = output / f"{label}.log"
        stderr_log = output / f"{label}.stderr.log"
        started = time.monotonic()
        record = {"argv": argv, "cwd": str(cwd), "phase": phase, "log": str(log), "stderr_log": str(stderr_log)}
        records.append(record)
        save()
        try:
            with log.open("w") as stream, stderr_log.open("w") as errors:
                completed = subprocess.run(argv, cwd=cwd, env=env or os.environ, stdout=stream, stderr=errors)
            record["exit_code"] = completed.returncode
        finally:
            elapsed = round(time.monotonic() - started, 3)
            record["elapsed_seconds"] = elapsed
            report["timings"][phase] = round(report["timings"][phase] + elapsed, 3)
            save()
        if completed.returncode:
            raise RuntimeError(f"Command exited {completed.returncode}; see {log} and {stderr_log}")
        return log.read_text(errors="replace")

    try:
        metadata = json.loads(run(["cargo", "metadata", "--locked", "--no-deps", "--format-version", "1"],
                                  "metadata", "feature_resolution", report["preparation"]))
        rustlib = run([os.environ.get("RUSTC", "rustc"), "--print", "target-libdir"],
                      "rustlib", "feature_resolution", report["preparation"]).strip()
        if not Path(rustlib).is_absolute() or not Path(rustlib).is_dir():
            raise ValueError(f"Invalid Rust runtime library directory: {rustlib}")
        contexts = {}
        for row in rows:
            contexts.setdefault((row["package"], parse_target(row)[2]), []).append(row)
        graphs = {}
        print(f"PREPARE {len(rows)} security groups in {len(contexts)} package/feature contexts", flush=True)
        for index, context in enumerate(contexts):
            graphs[context] = parse_feature_graph(run(tree_command([context]), f"features-{index:02}",
                                                      "feature_resolution", report["preparation"]))
        batches = []
        for context, graph in graphs.items():
            for batch in batches:
                # A package must never acquire the features from a different
                # selection context merely because it shares this build.
                if any(existing[0] == context[0] for existing in batch["contexts"]):
                    continue
                if graphs_compatible(batch["graph"], graph):
                    batch["contexts"].append(context)
                    batch["graph"] = merge_feature_graphs([batch["graph"], graph])
                    break
            else:
                batches.append({"contexts": [context], "graph": graph})
        report["batches"] = []
        artifacts = {}
        for index, batch in enumerate(batches):
            print(f"BUILD {index + 1}/{len(batches)}: " + ", ".join(package for package, _ in batch["contexts"]), flush=True)
            if len(batch["contexts"]) > 1:
                actual = parse_feature_graph(run(tree_command(batch["contexts"]), f"batch-{index:02}-features",
                                                  "feature_resolution", report["preparation"]))
                if actual != batch["graph"]:
                    raise ValueError(f"Batch {index} changes resolved dependency features; refusing to build")
            batch_rows = [row for context in batch["contexts"] for row in contexts[context]]
            report["batches"].append({"groups": [row["id"] for row in batch_rows],
                                      "feature_graph": feature_graph_json(batch["graph"])})
            raw = run(build_command(batch_rows), f"batch-{index:02}-build", "build", report["preparation"])
            messages = [json.loads(line) for line in raw.splitlines() if line.strip()]
            matched = match_artifacts(messages, batch_rows, metadata["packages"])
            library_paths = build_library_paths(messages, metadata["target_directory"])
            for artifact in matched.values():
                artifact["library_paths"] = library_paths
            artifacts.update(matched)
        listings = {}
        selections_by_group = {}
        unique_tests = set()
        for row, entry in zip(rows, report["groups"]):
            artifact = artifacts[row["id"]]
            executable = artifact["executable"]
            entry["artifact"] = artifact
            if not Path(executable).is_file() or not os.access(executable, os.X_OK):
                raise ValueError(f"Cargo test executable is missing or not executable: {executable}")
            if executable not in listings:
                listing = run([executable, "--list", "--format=pretty"], f"binary-{len(listings):02}-list", "listing",
                              report["preparation"], cwd=artifact["cwd"],
                              env=binary_environment(executable, metadata["target_directory"], artifact["library_paths"], rustlib))
                listings[executable] = (listing, report["preparation"][-1])
            listing, listing_record = listings[executable]
            entry["listing_log"] = listing_record["log"]
            exact = resolve_tests(row["tests"], listing)
            for name in exact:
                identity = (artifact["package_id"], tuple(artifact["target_kind"]), artifact["target_name"],
                            tuple(sorted(artifact["features"])), name)
                if identity in unique_tests:
                    raise ValueError(f"Security test selected by multiple groups: {identity}")
                unique_tests.add(identity)
            selections_by_group[row["id"]] = exact
            entry["tests"] = exact
        save()
        for row, entry in zip(rows, report["groups"]):
            entry["status"] = "running"
            save()
            print(f"RUN {row['id']}", flush=True)
            artifact = artifacts[row["id"]]
            exact = selections_by_group[row["id"]]
            try:
                log = run([artifact["executable"], "--exact", "--format=pretty", "--color=never", *exact],
                          f"{row['id']}.run", "execution", entry["commands"], cwd=artifact["cwd"],
                          env=binary_environment(artifact["executable"], metadata["target_directory"], artifact["library_paths"], rustlib))
                passed = validate_results(log, exact)
                entry.update(status="passed", passed_tests=passed)
                print(f"PASS {row['id']}: {passed}", flush=True)
            except (OSError, ValueError, RuntimeError) as error:
                entry.update(status="failed", error=str(error))
                print(f"FAIL {row['id']}: {error}", flush=True)
            save()
    except (OSError, ValueError, KeyError, RuntimeError) as error:
        report["preparation_error"] = str(error)
        print(f"FAIL preparation: {error}", flush=True)
    except KeyboardInterrupt:
        report["interrupted"] = True
        for entry in report["groups"]:
            if entry["status"] == "running":
                entry["status"] = "interrupted"
    report["elapsed_seconds"] = round(time.monotonic() - started, 3)
    report["status"] = "passed" if (report["groups"] and "preparation_error" not in report
                                      and not report.get("interrupted")
                                      and all(r["status"] == "passed" for r in report["groups"])) else "failed"
    save()
    print("Timings (seconds): " + json.dumps(report["timings"], sort_keys=True), flush=True)
    print(f"Report: {report_path}", flush=True)
    return 0 if report["status"] == "passed" else 1


if __name__ == "__main__":
    sys.exit(main())
