#!/usr/bin/env python3
"""Synthetic lifecycle tests; all binaries and state live in temporary dirs."""
from __future__ import annotations

import importlib.util
import io
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import time
import unittest
from contextlib import redirect_stdout, redirect_stderr
from unittest import mock

SCRIPT_DIR = Path(__file__).resolve().parent
SPEC = importlib.util.spec_from_file_location("fin_dev", SCRIPT_DIR / "fin-dev.py")
fin_dev = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = fin_dev
SPEC.loader.exec_module(fin_dev)


BROKER_SOURCE = r'''#!/usr/bin/env python3
import json, os, socket, sys
import fcntl
from pathlib import Path
def opt(name): return sys.argv[sys.argv.index(name)+1]
if os.environ.get("FIN_DEV_TEST_FAIL_BROKER"): raise SystemExit(2)
sock_path, db, identity, build_id = opt("--socket"), Path(opt("--database")), opt("--identity"), opt("--build-id")
mode = opt("--mode")
lock_file=open(db.parent/"broker.lock","a+")
fcntl.flock(lock_file,fcntl.LOCK_EX|fcntl.LOCK_NB)
if "--import-credentials" in sys.argv:
    credential_path=Path(opt("--import-credentials"))
    if mode != "live" or not credential_path.is_file(): raise SystemExit(2)
    config=Path(opt("--config"))
    try: saved=json.loads(config.read_text())
    except FileNotFoundError: saved={}
    saved.update({"mode":"live","credential_ref":"opaque-test-reference"})
    config.write_text(json.dumps(saved))
    print("Development credentials imported.")
    raise SystemExit(0)
try: db.read_text()
except FileNotFoundError: db.write_text("{}")
try: os.unlink(sock_path)
except FileNotFoundError: pass
listener=socket.socket(socket.AF_UNIX); listener.bind(sock_path); listener.listen(8)
while True:
    peer,_=listener.accept()
    with peer:
        reader=peer.makefile("rwb", buffering=0)
        try: hello=json.loads(reader.readline())
        except Exception: continue
        reader.write((json.dumps({"kind":"hello","identity":identity,"build_id":build_id,"mode":mode})+"\n").encode())
        if hello.get("kind") != "hello" or hello.get("identity") != identity or hello.get("build_id") != build_id or hello.get("mode") != mode: continue
        try: req=json.loads(reader.readline())
        except Exception: continue
        if req.get("identity") != identity or req.get("build_id") != build_id or req.get("mode") != mode: continue
        req=req.get("request",{})
        state=json.loads(db.read_text())
        if req.get("command")=="execute":
            state[req["id"]]={"status":"outcome_unknown","id":req["id"]}
            db.write_text(json.dumps(state))
            result=state[req["id"]]
        elif req.get("command")=="status": result=state.get(req["id"],{"status":"missing"})
        else: result={"status":"ok"}
        reader.write((json.dumps({"identity":identity,"build_id":build_id,"mode":mode,"response":{"result":result}})+"\n").encode())
'''

CLI_SOURCE = r'''#!/usr/bin/env python3
import json, os, socket, sys
identity=sys.argv[sys.argv.index("--identity")+1]
args=sys.argv[1:]
build_id=sys.argv[sys.argv.index("--build-id")+1]
mode=sys.argv[sys.argv.index("--mode")+1]
for flag in ("--identity", "--build-id", "--mode"):
    if flag in args:
        pos=args.index(flag); del args[pos:pos+2]
command=args[0] if args else "help"
id_value=args[args.index("--id")+1] if "--id" in args else (args[1] if len(args)>1 else "")
sock=socket.socket(socket.AF_UNIX); sock.connect(os.environ["FIN_DEV_SOCKET"])
f=sock.makefile("rwb", buffering=0)
f.write((json.dumps({"kind":"hello","identity":identity,"build_id":build_id,"mode":mode})+"\n").encode())
response=json.loads(f.readline())
if response != {"kind":"hello","identity":identity,"build_id":build_id,"mode":mode}: raise SystemExit(42)
f.write((json.dumps({"identity":identity,"build_id":build_id,"mode":mode,"request":{"command":command,"id":id_value}})+"\n").encode())
reply=json.loads(f.readline())
if reply.get("identity") != identity or reply.get("build_id") != build_id or reply.get("mode") != mode: raise SystemExit(43)
result=reply["response"]["result"]
if "--json" in args: print(json.dumps(result,separators=(",",":")))
else: print("result:"+result["status"])
'''


class FinDevTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="fin-dev-test-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.old_repo = fin_dev.REPO
        self.old_broker_rel, self.old_cli_rel = fin_dev.BROKER_REL, fin_dev.CLI_REL
        self.old_env = os.environ.copy()
        self.addCleanup(self.restore)
        self.addCleanup(self.cleanup_envs)
        self.state = self.root / "state"
        os.environ["FIN_DEV_STATE_HOME"] = str(self.state)
        self.repo_a = self.make_repo("worktree one")
        self.repo_b = self.make_repo("worktree two")
        # Every test, including cancellation before explicit selection, must
        # build only inside a disposable checkout.
        fin_dev.REPO = self.repo_a
        for repo, variant in ((self.repo_a, "a"), (self.repo_b, "b")):
            source_dir = repo / "crates/fin-cli/src"
            source_dir.mkdir(parents=True)
            (source_dir / "variant.rs").write_text(f"// build variant {variant}\n")
        self.cargo_dir = self.root / "bin"
        self.cargo_dir.mkdir()
        cargo = self.cargo_dir / "cargo"
        cargo.write_text("""#!/usr/bin/env python3
from pathlib import Path
import os
root=Path.cwd()
assert root.resolve().is_relative_to(Path(os.environ['TEST_REPOS_ROOT']).resolve()), 'mock cargo escaped its disposable checkout'
for rel,src in [(Path('target/debug/examples/fin-dev-broker'),os.environ['TEST_BROKER_SOURCE']), (Path('target/debug/examples/fin-dev'),os.environ['TEST_CLI_SOURCE'])]:
 p=root/rel; p.parent.mkdir(parents=True,exist_ok=True); p.write_text(Path(src).read_text()+'\\n# variant '+root.name+'\\n'); p.chmod(0o700)
""")
        cargo.chmod(0o700)
        os.environ["PATH"] = str(self.cargo_dir) + os.pathsep + self.old_env["PATH"]
        (self.root / "broker.py").write_text(BROKER_SOURCE)
        (self.root / "cli.py").write_text(CLI_SOURCE)
        os.environ["TEST_BROKER_SOURCE"] = str(self.root / "broker.py")
        os.environ["TEST_CLI_SOURCE"] = str(self.root / "cli.py")
        os.environ["TEST_REPOS_ROOT"] = str(self.root)

    def restore(self):
        fin_dev.REPO = self.old_repo
        fin_dev.BROKER_REL, fin_dev.CLI_REL = self.old_broker_rel, self.old_cli_rel
        os.environ.clear()
        os.environ.update(self.old_env)

    def cleanup_envs(self):
        for repo in (self.repo_a, self.repo_b):
            fin_dev.REPO = repo
            for name in ("alpha", "beta", "machine", "failure", "cancelled", "cancel-import", "live", "live-mode", "live-import", "solana-rehearsal", "live-fixture"):
                try:
                    fin_dev.stop(name)
                except fin_dev.DevError:
                    pass
                try:
                    fin_dev.reset(name)
                except fin_dev.DevError:
                    pass

    def make_repo(self, name):
        repo = self.root / name
        repo.mkdir()
        return repo

    def start_at(self, repo, name, mode="synthetic"):
        fin_dev.REPO = repo
        return fin_dev.start(name, False, mode)

    def invoke_at(self, repo, name, *args):
        fin_dev.REPO = repo
        stdout, stderr = io.StringIO(), io.StringIO()
        real_run = subprocess.run
        captured = {}
        def run_capture(*command, **kwargs):
            if command and isinstance(command[0], (list, tuple)) and command[0][0] == "ps":
                return real_run(*command, **kwargs)
            result = real_run(*command, **kwargs, capture_output=True, text=True, timeout=10)
            # Captures subprocess output for assertions without requiring a TTY.
            captured["stdout"], captured["stderr"] = result.stdout, result.stderr
            return result
        with mock.patch.object(fin_dev.subprocess, "run", side_effect=run_capture):
            with redirect_stdout(stdout), redirect_stderr(stderr):
                try:
                    code = fin_dev.cli(name, list(args))
                except fin_dev.DevError as error:
                    stderr.write(f"{error.code}: {error}\n")
                    code = 1
        return subprocess.CompletedProcess([], code, captured.get("stdout", stdout.getvalue()),
                                           captured.get("stderr", stderr.getvalue()))

    def test_embedded_kraken_contracts_change_build_identity_but_reports_do_not(self):
        fin_dev.REPO = self.repo_a
        snapshots = self.repo_a / "docs/kraken"
        snapshots.mkdir(parents=True)
        inputs = ("contracts.json", "kraken-ccxt.json", "krakenfutures-ccxt.json")
        for name in inputs:
            (snapshots / name).write_text('{"source":"original"}\n')
        original = fin_dev.source_build_id()
        (snapshots / "evidence-matrix.json").write_text('{"status":"passed"}\n')
        self.assertEqual(fin_dev.source_build_id(), original)
        for name in inputs:
            source = snapshots / name
            source.write_text('{"source":"changed"}\n')
            self.assertNotEqual(fin_dev.source_build_id(), original)
            source.write_text('{"source":"original"}\n')
            self.assertEqual(fin_dev.source_build_id(), original)

    def test_compiled_coinbase_matrix_and_inventory_change_build_identity(self):
        fin_dev.REPO = self.repo_a
        snapshots = self.repo_a / "docs/coinbase"
        snapshots.mkdir(parents=True)
        for name in ("contracts.json", "pinned-ccxt-4.5.85.json"):
            source = snapshots / name
            original = fin_dev.source_build_id()
            source.write_text('{"matrix_version":1}\n')
            self.assertNotEqual(fin_dev.source_build_id(), original)
            source.unlink()
            self.assertEqual(fin_dev.source_build_id(), original)

    def test_two_worktrees_run_distinct_builds_and_same_ids_are_isolated(self):
        first = self.start_at(self.repo_a, "alpha")
        second = self.start_at(self.repo_b, "alpha")
        self.assertNotEqual(first["buildId"], second["buildId"])
        one = self.invoke_at(self.repo_a, "alpha", "execute", "--id", "same-request", "--json")
        two = self.invoke_at(self.repo_b, "alpha", "execute", "--id", "same-request", "--json")
        self.assertEqual(one.returncode, 0, one.stderr)
        self.assertEqual(two.returncode, 0, two.stderr)
        self.assertEqual(json.loads(one.stdout)["status"], "outcome_unknown")
        self.assertEqual(json.loads(two.stdout)["status"], "outcome_unknown")
        fin_dev.REPO = self.repo_a
        dir_a = fin_dev.env_dir("alpha")
        fin_dev.REPO = self.repo_b
        dir_b = fin_dev.env_dir("alpha")
        self.assertNotEqual(dir_a, dir_b)
        self.assertEqual(json.loads((dir_a / "operations.sqlite").read_text()),
                         {"same-request": {"status": "outcome_unknown", "id": "same-request"}})
        self.assertEqual(json.loads((dir_b / "operations.sqlite").read_text()),
                         {"same-request": {"status": "outcome_unknown", "id": "same-request"}})

    def test_restart_recovers_uncertain_state_stop_and_reset_are_independent(self):
        self.start_at(self.repo_a, "alpha")
        self.start_at(self.repo_a, "beta")
        self.assertEqual(self.invoke_at(self.repo_a, "alpha", "execute", "--id", "uncertain").returncode, 0)
        alpha_dir = fin_dev.env_dir("alpha")
        alpha_db = alpha_dir / "operations.sqlite"
        before = alpha_db.read_bytes()
        with self.assertRaises(fin_dev.DevError) as raised:
            fin_dev.reset("alpha")
        self.assertEqual(raised.exception.code, "ENVIRONMENT_RUNNING")
        fin_dev.stop("alpha")
        self.assertEqual(alpha_db.read_bytes(), before)
        self.assertEqual(self.invoke_at(self.repo_a, "alpha", "status", "uncertain", "--json").returncode, 1)
        self.start_at(self.repo_a, "alpha")
        recovered = self.invoke_at(self.repo_a, "alpha", "status", "uncertain", "--json")
        self.assertEqual(recovered.returncode, 0, recovered.stderr)
        self.assertEqual(json.loads(recovered.stdout)["status"], "outcome_unknown")
        fin_dev.stop("alpha")
        self.assertTrue(fin_dev.env_dir("beta").exists())
        fin_dev.reset("alpha")
        self.assertFalse(alpha_dir.exists())
        self.assertTrue(fin_dev.env_dir("beta").exists())

    def test_solana_fixture_is_synthetic_only_and_bound_to_environment(self):
        started = fin_dev.start("solana-rehearsal", False, fixture="solana")
        self.assertEqual(started["status"], "running")
        metadata = fin_dev.read_environment(fin_dev.env_dir("solana-rehearsal"))
        self.assertEqual(metadata["fixture"], "solana")
        fin_dev.stop("solana-rehearsal")
        with self.assertRaises(fin_dev.DevError) as mismatch:
            fin_dev.start("solana-rehearsal", False)
        self.assertEqual(mismatch.exception.code, "DEV_FIXTURE_MISMATCH")
        with self.assertRaises(fin_dev.DevError) as live:
            fin_dev.start("live-fixture", False, mode="live", fixture="solana")
        self.assertEqual(live.exception.code, "DEV_FIXTURE_INVALID")

    def test_unavailable_mismatch_and_cross_worktree_routing_fail_closed(self):
        self.start_at(self.repo_a, "alpha")
        absent = self.invoke_at(self.repo_b, "alpha", "capabilities", "--json")
        self.assertEqual(absent.returncode, 1)
        self.assertIn("ENVIRONMENT_ABSENT", absent.stderr)
        fin_dev.REPO = self.repo_a
        directory = fin_dev.env_dir("alpha")
        marker = json.loads((directory / "broker.process.json").read_text())
        os.kill(int(marker["pid"]), 9)
        time.sleep(0.05)
        unavailable = self.invoke_at(self.repo_a, "alpha", "operations", "--json")
        self.assertEqual(unavailable.returncode, 1)
        self.assertIn("BROKER_UNAVAILABLE", unavailable.stderr)
        # Stale/reused process metadata is never permission to signal another PID.
        fin_dev.stop("alpha")
        fin_dev.reset("alpha")

    def test_missing_marker_and_unexpected_identity_never_signal_or_reset(self):
        self.start_at(self.repo_a, "alpha")
        directory = fin_dev.env_dir("alpha")
        marker_path = directory / "broker.process.json"
        marker = json.loads(marker_path.read_text())
        marker_path.unlink()
        with self.assertRaises(fin_dev.DevError) as missing:
            fin_dev.reset("alpha")
        self.assertEqual(missing.exception.code, "ENVIRONMENT_RUNNING")
        marker_path.write_text(json.dumps(marker))
        wrong = dict(marker, identity="copied-from-another-environment")
        marker_path.write_text(json.dumps(wrong))
        with self.assertRaises(fin_dev.DevError) as mismatch:
            fin_dev.stop("alpha")
        self.assertEqual(mismatch.exception.code, "PROCESS_OWNERSHIP_MISMATCH")
        self.assertTrue(fin_dev.process_matches(wrong))
        marker_path.write_text(json.dumps(marker))
        fin_dev.stop("alpha")
        fin_dev.reset("alpha")

    def test_start_failure_and_cancellation_leave_no_owned_broker(self):
        os.environ["FIN_DEV_TEST_FAIL_BROKER"] = "1"
        with self.assertRaises(fin_dev.DevError) as failed:
            self.start_at(self.repo_a, "failure")
        self.assertEqual(failed.exception.code, "BROKER_START_FAILED")
        os.environ.pop("FIN_DEV_TEST_FAIL_BROKER")
        original_wait = fin_dev.wait_ready
        def cancel(*_): raise KeyboardInterrupt()
        fin_dev.wait_ready = cancel
        try:
            with self.assertRaises(fin_dev.DevError) as cancelled:
                self.start_at(self.repo_a, "cancelled")
            self.assertEqual(cancelled.exception.code, "START_CANCELLED")
        finally:
            fin_dev.wait_ready = original_wait
        directory = fin_dev.env_dir("cancelled")
        marker = fin_dev.read_marker(directory / "broker.process.json")
        self.assertFalse(marker and fin_dev.process_matches(marker))

    def test_lifecycle_json_and_redirected_cli_output(self):
        fin_dev.REPO = self.repo_a
        stdout, stderr = io.StringIO(), io.StringIO()
        with redirect_stdout(stdout), redirect_stderr(stderr):
            missing_code = fin_dev.main(["cli", "absent", "--", "capabilities", "--json"])
        self.assertEqual(missing_code, 1)
        self.assertEqual(stderr.getvalue(), "")
        self.assertEqual(json.loads(stdout.getvalue())["error"]["code"], "ENVIRONMENT_ABSENT")
        stdout.seek(0); stdout.truncate(0)
        stderr.seek(0); stderr.truncate(0)
        with redirect_stdout(stdout), redirect_stderr(stderr):
            code = fin_dev.main(["--json", "start", "machine"])
        self.assertEqual(code, 0, stdout.getvalue() + stderr.getvalue())
        payload = json.loads(stdout.getvalue())
        self.assertEqual(payload["status"], "running")
        self.assertEqual(stderr.getvalue(), "")
        cli_result = self.invoke_at(self.repo_a, "machine", "operations", "--json")
        self.assertEqual(cli_result.returncode, 0, cli_result.stdout + cli_result.stderr)
        self.assertEqual(json.loads(cli_result.stdout), {"status": "ok"})
        fin_dev.REPO = self.repo_a
        fin_dev.stop("machine")
        fin_dev.reset("machine")
        stdout.seek(0); stdout.truncate(0)
        stderr.seek(0); stderr.truncate(0)
        with mock.patch.object(fin_dev, "start", side_effect=KeyboardInterrupt()):
            with redirect_stdout(stdout), redirect_stderr(stderr):
                cancelled = fin_dev.main(["--json", "start", "cancelled-output"])
        self.assertEqual(cancelled, 1)
        self.assertEqual(stderr.getvalue(), "")
        self.assertEqual(json.loads(stdout.getvalue())["error"]["code"], "DEVELOPMENT_COMMAND_CANCELLED")

    def test_live_import_is_offline_redacted_and_survives_restart(self):
        fin_dev.REPO = self.repo_a
        credentials = self.root / "disposable.env"
        secret_fixture = "FIN_DEV_BINANCE_API_KEY=fixture-key\nFIN_DEV_BINANCE_SECRET=fixture-secret\n"
        credentials.write_text(secret_fixture)
        stdout, stderr = io.StringIO(), io.StringIO()
        with redirect_stdout(stdout), redirect_stderr(stderr):
            code = fin_dev.main([
                "--json", "import", "live-import", "--credentials-file", str(credentials)
            ])
        self.assertEqual(code, 0, stdout.getvalue() + stderr.getvalue())
        self.assertEqual(stderr.getvalue(), "")
        self.assertEqual(json.loads(stdout.getvalue()), {
            "ok": True, "status": "imported", "environment": "live-import", "mode": "live"
        })
        directory = fin_dev.env_dir("live-import")
        metadata = fin_dev.read_environment(directory)
        config_path = directory / "config.json"
        saved_config = config_path.read_bytes()
        self.assertEqual(metadata["mode"], "live")
        self.assertEqual(json.loads(saved_config), {
            "mode": "live", "credential_ref": "opaque-test-reference"
        })
        self.assertNotIn(secret_fixture.encode(), (directory / "environment.json").read_bytes())
        self.assertNotIn(b"fixture-secret", saved_config)

        started = self.start_at(self.repo_a, "live-import", "live")
        self.assertEqual(started["mode"], "live")
        cli_result = self.invoke_at(self.repo_a, "live-import", "status", "missing", "--json")
        self.assertEqual(cli_result.returncode, 0, cli_result.stderr)
        self.assertEqual(json.loads(cli_result.stdout)["status"], "missing")
        fin_dev.stop("live-import")
        self.assertEqual(config_path.read_bytes(), saved_config)
        with self.assertRaises(fin_dev.DevError) as refused:
            fin_dev.reset("live-import")
        self.assertEqual(refused.exception.code, "LIVE_RESET_UNSUPPORTED")
        self.assertTrue((directory / "operations.sqlite").exists())
        with self.assertRaises(fin_dev.DevError) as mode_mismatch:
            self.start_at(self.repo_a, "live-import", "synthetic")
        self.assertEqual(mode_mismatch.exception.code, "DEV_MODE_MISMATCH")

    def test_mode_switch_requires_explicit_synthetic_reset_and_import_requires_stopped(self):
        self.start_at(self.repo_a, "live-mode", "synthetic")
        with self.assertRaises(fin_dev.DevError) as active_import:
            fin_dev.import_credentials("live-mode", __file__)
        self.assertEqual(active_import.exception.code, "ENVIRONMENT_ALREADY_RUNNING")
        fin_dev.stop("live-mode")
        with self.assertRaises(fin_dev.DevError) as switch_without_reset:
            self.start_at(self.repo_a, "live-mode", "live")
        self.assertEqual(switch_without_reset.exception.code, "DEV_MODE_MISMATCH")
        fin_dev.reset("live-mode")
        live = self.start_at(self.repo_a, "live-mode", "live")
        self.assertEqual(live["mode"], "live")
        fin_dev.stop("live-mode")
        with self.assertRaises(fin_dev.DevError) as live_reset:
            fin_dev.reset("live-mode")
        self.assertEqual(live_reset.exception.code, "LIVE_RESET_UNSUPPORTED")

    def test_import_errors_are_machine_json_and_never_echo_file_path(self):
        fin_dev.REPO = self.repo_a
        missing = self.root / "private missing path.env"
        stdout, stderr = io.StringIO(), io.StringIO()
        with redirect_stdout(stdout), redirect_stderr(stderr):
            code = fin_dev.main([
                "--json", "import", "live", "--credentials-file", str(missing)
            ])
        self.assertEqual(code, 1)
        self.assertEqual(stderr.getvalue(), "")
        response = stdout.getvalue()
        self.assertNotIn(str(missing), response)
        self.assertNotIn("fixture-secret", response)
        self.assertEqual(json.loads(response)["error"]["code"], "CREDENTIAL_FILE_UNAVAILABLE")

    def test_identity_discovery_is_explicit_and_forwarded_to_importer(self):
        fin_dev.REPO = self.repo_a
        credentials = self.root / "discovery.env"
        credentials.write_text("FIN_DEV_BINANCE_API_KEY=fixture-key\n")
        real_popen = subprocess.Popen
        imports = []

        def capture_import(command, *args, **kwargs):
            if command and str(command[0]).endswith("fin-dev-broker"):
                imports.append(list(command))
            return real_popen(command, *args, **kwargs)

        with mock.patch.object(fin_dev.subprocess, "Popen", side_effect=capture_import):
            fin_dev.import_credentials("live-import", str(credentials))
            stdout, stderr = io.StringIO(), io.StringIO()
            with redirect_stdout(stdout), redirect_stderr(stderr):
                code = fin_dev.main(["--json", "import", "live-import",
                                     "--credentials-file", str(credentials),
                                     "--provider", "binance",
                                     "--discover-identities"])
        self.assertEqual(code, 0)
        self.assertEqual(stderr.getvalue(), "")
        self.assertEqual(json.loads(stdout.getvalue())["status"], "imported")
        self.assertEqual(len(imports), 2)
        self.assertNotIn("--discover-identities", imports[0])
        self.assertIn("--discover-identities", imports[1])
        self.assertIn("--provider", imports[1])
        self.assertEqual(imports[1][imports[1].index("--provider") + 1], "binance")
        self.assertNotIn("fixture-key", repr(imports))

    def test_live_import_cancellation_is_machine_safe_and_stops_owned_child(self):
        fin_dev.REPO = self.repo_a
        credentials = self.root / "cancel-import.env"
        credentials.write_text("FIN_DEV_BINANCE_SECRET=must-not-appear\n")
        process = mock.Mock()
        process.wait.side_effect = KeyboardInterrupt()
        process.poll.return_value = None
        real_popen = subprocess.Popen
        def selective_popen(command, *args, **kwargs):
            if command and str(command[0]).endswith("fin-dev-broker"):
                return process
            return real_popen(command, *args, **kwargs)
        stdout, stderr = io.StringIO(), io.StringIO()
        with mock.patch.object(fin_dev.subprocess, "Popen", side_effect=selective_popen), \
             mock.patch.object(fin_dev, "stop_import_process") as stop_child, \
             redirect_stdout(stdout), redirect_stderr(stderr):
            code = fin_dev.main([
                "--json", "import", "cancel-import", "--credentials-file", str(credentials)
            ])
        self.assertEqual(code, 1)
        self.assertEqual(stderr.getvalue(), "")
        response = stdout.getvalue()
        self.assertNotIn(str(credentials), response)
        self.assertNotIn("must-not-appear", response)
        self.assertEqual(json.loads(response)["error"]["code"], "CREDENTIAL_IMPORT_CANCELLED")
        stop_child.assert_called()


if __name__ == "__main__":
    unittest.main(verbosity=2)
