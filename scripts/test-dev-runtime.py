#!/usr/bin/env python3
"""Actual Rust binaries in two source checkouts; synthetic state and no network."""
import hashlib
import json
import os
from pathlib import Path
import shutil
import signal
import socket
import sqlite3
import subprocess
import sys
import tempfile
import threading
import unittest

REPO = Path(__file__).resolve().parent.parent


class RuntimeIsolation(unittest.TestCase):
    def test_live_import_uses_only_disposable_fixture_credentials_and_preserves_them(self):
        # Public test key, never funded. Only local management requests are sent:
        # constructing real adapters must not read installed state or call a venue.
        with tempfile.TemporaryDirectory(prefix="gofin-live-fixture-") as temporary:
            root = Path(temporary)
            env = dict(os.environ, GOFIN_DEV_STATE_HOME=str(root / "state"), NO_COLOR="1")
            env.pop("CARGO_TARGET_DIR", None)
            credential_file = root / "fixture.env"
            sentinel = "synthetic-binance-secret-do-not-log"
            credential_file.write_text(
                "GOFIN_DEV_EVM_PRIVATE_KEY=" + "0" * 63 + "1\n"
                "GOFIN_DEV_BINANCE_API_KEY=synthetic-binance-key\n"
                "GOFIN_DEV_BINANCE_SECRET=" + sentinel + "\n"
                "GOFIN_DEV_BINANCE_UID=123456789\n")
            credential_file.chmod(0o600)
            outputs = []

            def run(action, *args, name="live-fixture", success=True):
                command = [sys.executable, str(REPO / "scripts/gofin-dev.py")]
                if action != "cli":
                    command.append("--json")
                result = subprocess.run(command + [action, name, *args], env=env,
                                        capture_output=True, text=True, timeout=240)
                outputs.extend([result.stdout, result.stderr])
                self.assertEqual(result.returncode == 0, success, result.stdout + result.stderr)
                self.assertEqual(result.stderr, "")
                return json.loads(result.stdout)

            worktree = hashlib.sha256(str(REPO.resolve()).encode()).hexdigest()[:20]
            directory = root / "state" / worktree / "live-fixture"
            try:
                run("import", "--credentials-file", str(credential_file))
                run("import", "--credentials-file", str(credential_file), name="overlap")
                credential_file.unlink()  # The source is not required on restart.
                run("start", "--mode", "live")
                metadata = json.loads((directory / "environment.json").read_text())
                self.assertEqual(metadata["mode"], "live")
                capabilities = run("cli", "capabilities", "--json")
                self.assertFalse(capabilities["development"]["synthetic"])
                self.assertFalse(capabilities["development"]["touch_id_required"])
                self.assertTrue(capabilities["development"]["real_funds_possible"])
                self.assertEqual(capabilities["development"]["approval_mechanism"],
                                 "development-auto-approval-no-os-authentication")
                self.assertIn("wallet.transfer", {item["name"] for item in
                              capabilities["development"]["operations"]})
                connections = run("cli", "connections", "--json")
                self.assertIn("dev-wallet", json.dumps(connections))
                self.assertIn("dev-binance", json.dumps(connections))
                run("cli", "policy", "show", "--json")
                run("cli", "operations", "--json")
                # Even fully local management must reject the wrong mode.
                crossed = subprocess.run(
                    [str(directory / "runtime/gofin-dev"), "--identity", metadata["environment_identity"],
                     "--build-id", metadata["build_id"], "--mode", "synthetic", "operations", "--json"],
                    env=dict(env, GOFIN_DEV_SOCKET=metadata["socket"],
                             GOFIN_DEV_IDENTITY=metadata["environment_identity"],
                             GOFIN_DEV_BUILD_ID=metadata["build_id"]),
                    capture_output=True, text=True, timeout=10)
                self.assertNotEqual(crossed.returncode, 0)
                self.assertEqual(json.loads(crossed.stdout)["error"]["code"],
                                 "DEVELOPMENT_IDENTITY_MISMATCH")
                # The human coordinates shared-account tests. Both brokers may
                # load identical credentials while owning separate local state.
                run("start", "--mode", "live", name="overlap")
                overlap_directory = directory.parent / "overlap"
                overlap_metadata = json.loads((overlap_directory / "environment.json").read_text())
                for field in ("socket", "database", "config", "environment_id"):
                    self.assertNotEqual(metadata[field], overlap_metadata[field])
                self.assertEqual(connections, run("cli", "connections", "--json", name="overlap"))
                self.assertTrue(run("cli", "capabilities", "--json", name="overlap")["development"]["real_funds_possible"])
                run("start", "--mode", "live", success=False)  # Same environment still has one owner.
                run("import", "--credentials-file", str(credential_file), success=False)
                run("reset", success=False)
                run("stop")
                stored = {str(p.relative_to(directory)): hashlib.sha256(p.read_bytes()).hexdigest()
                          for p in (directory / "credentials").rglob("*") if p.is_file()}
                self.assertTrue(stored)
                run("start", "--mode", "synthetic", success=False)
                run("start", "--mode", "live")
                self.assertEqual(connections, run("cli", "connections", "--json"))
                self.assertEqual(stored, {str(p.relative_to(directory)): hashlib.sha256(p.read_bytes()).hexdigest()
                                          for p in (directory / "credentials").rglob("*") if p.is_file()})
                run("stop")
                run("reset", success=False)  # Live journals are never synthetic-reset targets.
                metadata_path = directory / "environment.json"
                retained_metadata = metadata_path.read_bytes()
                metadata_path.unlink()
                try:
                    run("reset", success=False)
                    self.assertTrue((directory / "operations.sqlite").exists())
                finally:
                    metadata_path.write_bytes(retained_metadata)
                # Stopping/restarting the first environment did not disturb the second.
                self.assertEqual(connections, run("cli", "connections", "--json", name="overlap"))
                self.assertEqual(run("cli", "operations", "--json", name="overlap"), [])
                run("stop", name="overlap")
                logs = "\n".join(p.read_text(errors="replace") for p in directory.glob("logs/*"))
                self.assertNotIn(sentinel, logs + "\n".join(outputs))
                self.assertNotIn("synthetic-binance-key", logs + "\n".join(outputs))
            finally:
                for name in ("live-fixture", "overlap"):
                    subprocess.run([sys.executable, str(REPO / "scripts/gofin-dev.py"),
                                    "--json", "stop", name], env=env,
                                   capture_output=True, timeout=15)

    def test_two_builds_recovery_and_identity(self):
        with tempfile.TemporaryDirectory(prefix="gofin-runtime-") as temporary:
            root = Path(temporary)
            other = root / "second-source"
            other.mkdir()
            # Copy source inputs only. Sequential builds share the already-built
            # dependency cache, while the runner snapshots each executable pair.
            for name in ("Cargo.toml", "Cargo.lock", "rust-toolchain.toml"):
                shutil.copy2(REPO / name, other / name)
            for name in ("crates", "scripts", "skills"):
                shutil.copytree(REPO / name, other / name,
                                ignore=shutil.ignore_patterns("__pycache__"))
            # These reviewed snapshots are compiled into gofin-kraken. The second
            # checkout must own its inputs instead of reusing cached originals.
            shutil.copytree(REPO / "docs/kraken", other / "docs/kraken",
                            ignore=shutil.ignore_patterns("__pycache__"))
            (other / "target").symlink_to(REPO / "target", target_is_directory=True)
            with (other / "crates/gofin-cli/examples/gofin-dev.rs").open("a") as source:
                source.write("\n// Distinct source snapshot for isolation verification.\n")
            env = dict(os.environ, GOFIN_DEV_STATE_HOME=str(root / "state"), NO_COLOR="1")
            env.pop("CARGO_TARGET_DIR", None)
            roots = (REPO, other)

            def run(source, action, *args, success=True):
                command = [sys.executable, str(source / "scripts/gofin-dev.py")]
                if action != "cli":
                    command.append("--json")
                result = subprocess.run(command + [action, "verify", *args],
                                        env=env, text=True, capture_output=True, timeout=240)
                self.assertEqual(result.returncode == 0, success,
                                 f"{command} {args}\n{result.stdout}\n{result.stderr}")
                if "--json" in args or action != "cli":
                    self.assertEqual(result.stderr, "")
                    value = json.loads(result.stdout)
                    self.assertNotIn("\x1b", result.stdout)
                    return value
                return result

            def metadata(source):
                key = hashlib.sha256(str(source.resolve()).encode()).hexdigest()[:20]
                directory = root / "state" / key / "verify"
                return directory, json.loads((directory / "environment.json").read_text())

            try:
                first = run(REPO, "start")
                second = run(other, "start")
                self.assertNotEqual(first["buildId"], second["buildId"])
                a_dir, a = metadata(REPO)
                b_dir, b = metadata(other)
                self.assertNotEqual(a["socket"], b["socket"])
                self.assertNotEqual(a["environment_identity"], b["environment_identity"])
                records = []
                for source in roots:
                    echo = run(source, "cli", "read", "synthetic.echo", "--input",
                               '{"value":"isolated"}', "--json")
                    self.assertEqual(echo["value"], "isolated")
                    record = run(source, "cli", "execute", "synthetic.uncertain-write",
                                 "--id", "same-logical-id", "--input", '{"value":"test"}', "--json")
                    self.assertEqual(record["status"], "unknown", record)
                    records.append(record)
                self.assertNotEqual(records[0]["id"], records[1]["id"])
                self.assertNotEqual(records[0]["submission"], records[1]["submission"])
                # B was rebuilt from another checkout; A's captured build still works.
                self.assertEqual(len(run(REPO, "cli", "operations", "--json")), 1)
                run(REPO, "reset", success=False)
                run(REPO, "stop")
                # Model an interrupted candidate in this stopped synthetic
                # environment. It has no dispatch identity or financial effects;
                # the existing submitted operation above must remain intact.
                orphan = dict(records[0], id="synthetic-orphan-approval",
                              request_id="synthetic-orphan-request",
                              fingerprint="synthetic-orphan-fingerprint",
                              status="awaiting_approval", approval=None,
                              submission=None, effects=None, error=None,
                              last_reconciliation_evidence=None)
                orphan.pop("dispatch_evidence", None)
                with sqlite3.connect(a_dir / "operations.sqlite") as database:
                    database.execute(
                        "INSERT INTO operations(id,request_id,fingerprint,operation,status,run_id,record_json,created_at,updated_at) VALUES (?,?,?,?,?,NULL,?,?,?)",
                        (orphan["id"], orphan["request_id"], orphan["fingerprint"],
                         orphan["operation"], orphan["status"], json.dumps(orphan),
                         orphan["created_at"], orphan["updated_at"]))
                run(REPO, "cli", "operations", "--json", success=False)
                self.assertEqual(len(run(other, "cli", "operations", "--json")), 1)
                run(REPO, "start")
                cancelled = run(REPO, "cli", "status", orphan["id"], "--json")
                self.assertEqual(cancelled["status"], "cancelled")
                self.assertEqual(cancelled["error"]["code"], "INTERRUPTED_APPROVAL")
                self.assertIsNone(cancelled["submission"])
                recovered = run(REPO, "cli", "status", records[0]["id"], "--json")
                self.assertEqual(recovered["status"], "completed")
                self.assertEqual(recovered["submission"], records[0]["submission"])
                retry = run(REPO, "cli", "execute", "synthetic.uncertain-write",
                            "--id", "same-logical-id", "--input", '{"value":"test"}', "--json")
                self.assertEqual(retry["id"], recovered["id"])
                self.assertEqual(len(list(a_dir.glob("**/synthetic-receipts/*.json"))), 1)
                a_dir, a = metadata(REPO)
                # Invoke A's real binary against B directly. Both peers are the
                # same OS user; the handshake must reject before dispatch.
                crossed_env = dict(env, GOFIN_DEV_SOCKET=b["socket"],
                                   GOFIN_DEV_IDENTITY=a["environment_identity"],
                                   GOFIN_DEV_BUILD_ID=a["build_id"])
                crossed = subprocess.run(
                    [str(a_dir / "runtime/gofin-dev"), "--identity", a["environment_identity"],
                     "--build-id", a["build_id"], "operations", "--json"],
                    env=crossed_env, text=True, capture_output=True, timeout=10)
                self.assertEqual(crossed.returncode, 1)
                self.assertEqual(json.loads(crossed.stdout)["error"]["code"],
                                 "DEVELOPMENT_IDENTITY_MISMATCH")
                self.assertEqual(crossed.stderr, "")
                # Isolate the environment comparison from the build comparison.
                wrong_environment = dict(crossed_env, GOFIN_DEV_SOCKET=a["socket"],
                                         GOFIN_DEV_IDENTITY=b["environment_identity"])
                crossed = subprocess.run(
                    [str(a_dir / "runtime/gofin-dev"), "--identity", b["environment_identity"],
                     "--build-id", a["build_id"], "operations", "--json"],
                    env=wrong_environment, text=True, capture_output=True, timeout=10)
                self.assertEqual(crossed.returncode, 1)
                self.assertEqual(json.loads(crossed.stdout)["error"]["code"],
                                 "DEVELOPMENT_IDENTITY_MISMATCH")
                # A missing lifecycle marker cannot authorize reset of a live DB.
                marker_path = a_dir / "broker.process.json"
                marker_bytes = marker_path.read_bytes()
                marker_path.unlink()
                try:
                    run(REPO, "reset", success=False)
                    self.assertTrue((a_dir / "operations.sqlite").exists())
                finally:
                    marker_path.write_bytes(marker_bytes)
                marker_path.write_bytes((b_dir / "broker.process.json").read_bytes())
                try:
                    run(REPO, "stop", success=False)
                    self.assertEqual(len(run(other, "cli", "operations", "--json")), 1)
                finally:
                    marker_path.write_bytes(marker_bytes)
                for args in (("daemon", "stop"), ("setup",), ("skill", "install", "--agent", "codex")):
                    denied = run(REPO, "cli", *args, "--json", success=False)
                    self.assertEqual(denied["error"]["code"], "DEVELOPMENT_COMMAND_UNAVAILABLE")
                for after_dispatch in (False, True):
                    with tempfile.TemporaryDirectory(prefix="gofin-signal-", dir="/tmp") as signal_dir:
                        sock_path = str(Path(signal_dir) / "peer.sock")
                        with socket.socket(socket.AF_UNIX) as listener:
                            listener.bind(sock_path)
                            listener.listen(1)
                            listener.settimeout(10)
                            reached, done = threading.Event(), threading.Event()
                            peer_errors = []

                            def stalled_peer():
                                try:
                                    peer, _ = listener.accept()
                                    with peer, peer.makefile("rwb", buffering=0) as wire:
                                        hello = json.loads(wire.readline())
                                        if after_dispatch:
                                            wire.write((json.dumps(hello) + "\n").encode())
                                            frame = json.loads(wire.readline())
                                            self.assertEqual(frame["request"]["method"], "execute")
                                        reached.set()
                                        done.wait(10)
                                except BaseException as error:
                                    peer_errors.append(error)
                                    reached.set()

                            thread = threading.Thread(target=stalled_peer)
                            thread.start()
                            process = subprocess.Popen(
                                [str(a_dir / "runtime/gofin-dev"), "--identity", a["environment_identity"],
                                 "--build-id", a["build_id"], "execute", "synthetic.uncertain-write",
                                 "--id", "cancel-fixture", "--input", '{"value":"cancel"}', "--json"],
                                env=dict(crossed_env, GOFIN_DEV_SOCKET=sock_path),
                                stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
                            try:
                                self.assertTrue(reached.wait(10), "CLI did not reach the expected protocol stage")
                                self.assertEqual(peer_errors, [])
                                process.send_signal(signal.SIGINT)
                                out, err = process.communicate(timeout=10)
                                self.assertEqual(err, "")
                                self.assertEqual(process.returncode, 1)
                                self.assertEqual(json.loads(out)["error"]["code"],
                                                 "OUTCOME_UNKNOWN" if after_dispatch else "CANCELLED")
                            finally:
                                done.set()
                                if process.poll() is None:
                                    process.kill()
                                    process.wait()
                                thread.join(timeout=10)
                human = run(REPO, "cli", "read", "synthetic.echo", "--input", '{"value":"human"}')
                self.assertIn("human", human.stdout)
                self.assertNotIn("\x1b", human.stdout + human.stderr)
                master, slave = os.openpty()
                try:
                    machine_tty = subprocess.run(
                        [sys.executable, str(REPO / "scripts/gofin-dev.py"), "cli", "verify",
                         "read", "synthetic.echo", "--input", '{"value":"tty"}',
                         "--json", "--verbose"], env=env, stdin=slave, stdout=slave,
                        stderr=slave, timeout=10)
                    self.assertEqual(machine_tty.returncode, 0)
                    tty_output = os.read(master, 65536).decode()
                    self.assertEqual(json.loads(tty_output)["value"], "tty")
                    self.assertNotIn("\x1b", tty_output)
                finally:
                    os.close(slave)
                    os.close(master)
                run(REPO, "stop")
                run(REPO, "reset")
                self.assertFalse(a_dir.exists())
                self.assertEqual(len(run(other, "cli", "operations", "--json")), 1)
            finally:
                for source in roots:
                    for action in ("stop", "reset"):
                        subprocess.run([sys.executable, str(source / "scripts/gofin-dev.py"),
                                        "--json", action, "verify"], env=env,
                                       capture_output=True, timeout=15)


if __name__ == "__main__":
    unittest.main(verbosity=2)
