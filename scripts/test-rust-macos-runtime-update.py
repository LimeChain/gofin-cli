#!/usr/bin/env python3
"""Runtime-hotfix regression fixtures; no privilege, real state, or GoFin execution.

Run: python3 scripts/test-rust-macos-runtime-update.py
The harness rewrites fixed system paths into one temporary tree and replaces OS,
privilege, process, and signature checks. It does not establish real root/Touch ID
behavior or make the advisory process checks a synchronization guarantee.
"""

import hashlib
import json
import plistlib
from pathlib import Path
import re
import shutil
import sqlite3
import stat
import subprocess
import sys
import tempfile
import unittest


SCRIPT = Path(__file__).resolve().with_name("update-rust-macos-runtime.sh")
FILES = ("gofin", "gofin-broker", "gofin-touch-id", "install.sh", "README.md", "BUILDINFO", "SHA256SUMS")
DRIVER = r'''
import json, os, pathlib, stat, sys
root = pathlib.Path(ROOT)
name = pathlib.Path(sys.argv[0]).name
args = sys.argv[1:]
options = json.loads((root / "options.json").read_text())
events_file = root / "events.jsonl"
# Most OS doubles only append events; load history only for stateful doubles.
events = ([json.loads(line) for line in events_file.read_text().splitlines()]
          if name in ("mv", "codesign", "pgrep", "launchctl") and events_file.exists() else [])
with events_file.open("a") as log:
    log.write(json.dumps([name, args]) + "\n")
# A rewriting mistake must fail before any real system or private-state access.
for argument in (args[1:] if name == "awk" and not args[0].startswith("-") else args):
    if argument.startswith("/"):
        if not argument.startswith(str(root) + "/"):
            sys.exit("Refusing non-fixture path: " + argument)
        protected_state = any(argument.endswith("/" + product + "/" + part) or "/" + product + "/" + part + "/" in argument
                              for product in ("Fin", "GoFin") for part in ("private", "run"))
        lock_metadata = (name in ("stat", "ls") and argument.endswith(("/private", "/private/maintenance.lock")))
        lock_open = name == "python3" and argument.endswith("/private/maintenance.lock")
        if protected_state and not (lock_metadata or lock_open):
            sys.exit("Refusing access to private state or service socket: " + argument)
if name in ("mkdir", "rmdir", "cp", "rm", "mv", "mktemp", "chmod", "sleep"):
    if name == "chmod" and args[0] == "-RN":
        sys.exit(0)
    if name == "cp" and args[-2] == str(root / "new bundle/gofin") and options.get("race_symlink"):
        broker = root / "new bundle/gofin-broker"
        broker.unlink()
        broker.symlink_to(root / "old bundle/gofin-broker")
    if name == "cp" and args[-2] == str(root / "new bundle/gofin") and options.get("race_helper"):
        import hashlib
        bundle = root / "new bundle"
        bundle.joinpath("gofin-touch-id").write_text("synthetic raced helper\n")
        bundle.joinpath("SHA256SUMS").write_text("".join(
            hashlib.sha256(bundle.joinpath(component).read_bytes()).hexdigest() + "  " + component + "\n"
            for component in ("gofin", "gofin-broker", "gofin-touch-id", "install.sh", "README.md", "BUILDINFO")))
    if name == "mv":
        import signal
        if args[-2:] == [str(root / "Library/Fin"), str(root / "Library/GoFin")]:
            if options.get("fail_root_move"): sys.exit(1)
            if options.get("signal_root_move"):
                os.replace(args[-2], args[-1])
                os.kill(os.getppid(), signal.SIGTERM)
                sys.exit(0)
        activation = "/incoming/" in args[-2] or pathlib.Path(args[-2]).name in ("incoming-GoFin", "incoming.plist")
        activation_count = 1 + sum(
            event[0] == "mv" and ("/incoming/" in event[1][-2] or pathlib.Path(event[1][-2]).name in ("incoming-GoFin", "incoming.plist"))
            for event in events)
        if activation and options.get("fail_activation") == activation_count:
            sys.exit(1)
        if ("/restore/" in args[-2] or pathlib.Path(args[-2]).name == "restore-GoFin") and options.get("fail_restore"):
            sys.exit(1)
        if activation and options.get("signal_activation") == activation_count:
            os.replace(args[-2], args[-1])
            os.kill(os.getppid(), signal.SIGTERM)
            sys.exit(0)
    path = ("/usr/bin/" if name == "mktemp" else "/bin/") + name
    os.execv(path, [name] + args)
elif name in ("awk", "shasum", "cmp", "grep", "tr"):
    os.execv("/usr/bin/" + name, [name] + args)
elif name == "python3":
    os.execv(sys.executable, [sys.executable] + args)
elif name == "dirname":
    print(os.path.dirname(args[-1]))
elif name == "uname":
    print("Darwin" if args == ["-s"] else "arm64")
elif name == "id":
    print(options.get("uid", "0"))
elif name == "stat":
    if args[1] == "%d":
        print(options.get("devices", {}).get(args[-1], "0"))
    elif args[1] == "%Lp":
        print(format(stat.S_IMODE(pathlib.Path(args[-1]).stat().st_mode), "o"))
    elif args[1] == "%g":
        print(options.get("groups", {}).get(args[-1], "0"))
    else:
        print(options.get("owners", {}).get(args[-1], "0"))
elif name == "ls":
    path = pathlib.Path(args[-1])
    mode = stat.filemode(path.stat().st_mode)
    print(mode + ("+" if str(path) in options.get("acls", []) else ""), "1 root wheel 0 Jan 1 00:00", path)
elif name == "codesign":
    if (options.get("post_replace_signature_failure") and
            args[-1].startswith(str(root / "Library/GoFin/runtime") + "/") and
            any(event[0] == "mv" for event in events)):
        sys.exit(1)
    if "/incoming/" in args[-1] or pathlib.Path(args[-1]).name == "incoming-GoFin":
        if options.get("bad_signature") and "--verify" in args:
            sys.exit(1)
        if options.get("no_hardened") and "-d" in args:
            print("CodeDirectory flags=0x0(none)")
            sys.exit(0)
    if "-d" in args:
        print("CodeDirectory flags=0x10000(runtime)")
elif name == "otool":
    library = "/usr/lib/libSystem.B.dylib"
    if "/incoming/" in args[-1] and options.get("non_os_library"):
        library = "/opt/homebrew/lib/unsafe.dylib"
    print(args[-1] + ":\n\t" + library + " (compatibility version 1.0.0)")
elif name == "pgrep":
    if options.get("pgrep_error"):
        sys.exit(2)
    if args[0] == "-x" and options.get("active_client"):
        sys.exit(0)
    if args[0] == "-f" and options.get("active_enrollment"):
        sys.exit(0)
    if args[0] == "-f" and options.get("late_enrollment") and any(event[0] == "mktemp" for event in events):
        sys.exit(0)
    if args == ["-x", "fin-broker|gofin-broker"] and options.get("active_broker"):
        sys.exit(0)
    # A persistent broker daemon by itself is deliberately allowed.
    sys.exit(1)
elif name == "plutil":
    import plistlib
    value = plistlib.loads(pathlib.Path(args[-1]).read_bytes())[args[1]]
    print(json.dumps(value) if args[2] == "json" else value)
elif name == "launchctl":
    if args[0] == "print":
        if args[1] == "system/io.gofin-cli.broker" and (root / "Library/Fin").exists():
            sys.exit(113)
        if options.get("public_running") and not (options.get("missing_public_restart") and any(event[0] == "launchctl" and event[1][0] == "kickstart" for event in events)):
            print("system/io.gofin-cli.broker = {\n\tstate = running\n\tpid = 1234\n}")
        sys.exit(0)
    if args[0] == "kickstart" and options.get("fail_public_kickstart_once") and sum(event[0] == "launchctl" and event[1][0] == "kickstart" for event in events) == 0:
        sys.exit(1)
    if args[0] == "bootstrap" and options.get("fail_public_bootstrap_once") and sum(event[0] == "launchctl" and event[1][0] == "bootstrap" for event in events) == 0:
        sys.exit(1)
    if args[0] == "bootstrap" and options.get("fail_bootstrap"):
        # Fail only the new registration, allowing old registration restoration.
        import plistlib
        if plistlib.loads(pathlib.Path(args[-1]).read_bytes())["Label"] == "io.gofin-cli.broker":
            sys.exit(1)
elif name == "sudo":
    if args[-2:] == ["internal-completions", "refresh"]:
        sys.exit(1 if options.get("completion_refresh_failure") else 0)
    sys.exit(1 if options.get("skill_refresh_failure") else 0)
elif name != "chown":
    sys.exit("Unexpected mock command: " + name)
'''


class RuntimeUpdateFixture(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="gofin-runtime-update-test-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name).resolve()
        self.old = self.root / "old bundle"
        self.new = self.root / "new bundle"
        self.runtime = self.root / "Library/GoFin/runtime"
        self.private = self.root / "Library/GoFin/private"
        self.lock = self.root / "Library/GoFin/.runtime-update.lock"
        self.bin = self.root / "bin"
        self.options = {}
        for path in (self.old, self.new, self.runtime, self.private, self.bin):
            path.mkdir(parents=True)
        for path in (self.root / "Library", self.runtime.parent, self.runtime):
            path.chmod(0o755)
        for name in FILES[:-1]:
            self.old.joinpath(name).write_text("synthetic old " + name + "\n")
            self.old.joinpath(name).chmod(0o755 if name in FILES[:4] else 0o644)
        self.old.joinpath("BUILDINFO").write_text("source_commit=synthetic\nsource_state=modified\narchitecture=arm64\n")
        self.manifest(self.old)
        for name in FILES:
            shutil.copy2(self.old / name, self.new / name)
            shutil.copy2(self.old / name, self.runtime / name)
        authorization = self.runtime / "authorization"
        authorization.mkdir()
        shutil.copy2(self.runtime / "gofin-touch-id", authorization / "GoFin")
        authorization.chmod(0o755)
        (authorization / "GoFin").chmod(0o755)
        self.new.joinpath("gofin").write_text("synthetic new discovery\n")
        self.new.joinpath("gofin-broker").write_text("synthetic new enrollment\n")
        self.install_fake_broker(self.old / "gofin-broker", "old")
        self.install_fake_broker(self.new / "gofin-broker", "new")
        shutil.copy2(self.old / "gofin-broker", self.runtime / "gofin-broker")
        self.manifest(self.old)
        self.manifest(self.runtime)
        self.new.joinpath("BUILDINFO").write_text("source_commit=synthetic-patch\nsource_state=modified\narchitecture=arm64\n")
        self.manifest(self.new)
        self.private.joinpath("maintenance.lock").touch(mode=0o600)
        self.private.joinpath("config.json").write_bytes(b"synthetic private config\x00")
        with sqlite3.connect(self.private / "operations.sqlite") as connection:
            connection.execute("PRAGMA user_version=4")
            connection.execute("CREATE TABLE synthetic_unresolved_record (id TEXT)")
            connection.execute("INSERT INTO synthetic_unresolved_record VALUES ('original-pending-submission-id')")
        connection.close()
        for name in ('policies.json', 'history.json', 'pending-identities.json', 'reservations.json'):
            self.private.joinpath(name).write_bytes(b'synthetic protected sentinel ' + name.encode() + b'\x00')
        self.private.joinpath("credentials").mkdir()
        self.private.joinpath("credentials/synthetic-only").write_bytes(b"synthetic credential\x00")
        self.private_before = {str(path.relative_to(self.private)): path.read_bytes()
                               for path in self.private.rglob("*") if path.is_file()}
        driver = "#!" + sys.executable + "\n" + DRIVER.replace("ROOT", repr(str(self.root)))
        for name in ("uname", "id", "stat", "ls", "codesign", "otool", "pgrep", "chown", "python3", "dirname",
                     "mkdir", "rmdir", "cp", "rm", "mv", "mktemp", "chmod", "awk", "shasum", "cmp", "sudo", "launchctl", "sleep", "plutil", "tr", "grep"):
            self.bin.joinpath(name).write_text(driver)
            self.bin.joinpath(name).chmod(0o755)
        source = SCRIPT.read_text()
        terminal_guard = '[ -t 0 ] && [ -t 1 ] && [ -t 2 ]'
        self.assertEqual(source.count(terminal_guard), 1)
        # There is no production fixture flag/environment override.
        source = source.replace(terminal_guard, "true").replace("expected_uid = 0", "expected_uid = os.geteuid()")
        source = re.sub(r'(?<![\w/])/(?:Library|private)(?=[/\s";:)]|$)',
                        lambda match: str(self.root) + match.group(0), source)
        source = re.sub(r'(?<![\w/])/(?:usr/)?s?bin/([a-z]+)',
                        lambda match: str(self.bin / match.group(1)), source)
        self.script = self.root / "update.sh"
        self.script.write_text(source)
        checker = SCRIPT.with_name("check-runtime-store-compatibility.py").read_text()
        checker = re.sub(r'(?<![\w/])/(?:Library|private)(?=[/\s";:)]|$)',
                         lambda match: str(self.root) + match.group(0), checker)
        checker = checker.replace("if os.geteuid() != 0:", "if False:")
        checker = checker.replace("expected_uid=0)", "expected_uid=os.geteuid())")
        self.root.joinpath("check-runtime-store-compatibility.py").write_text(checker)

    def prepare_legacy_layout(self, *, name="fin", namespace=True):
        self.target_runtime = self.root / "Library/GoFin/runtime"
        self.target_plist = self.root / "Library/LaunchDaemons/io.gofin-cli.broker.plist"
        self.target_registration = self.root / "private/etc/paths.d/gofin"
        if namespace:
            self.runtime.parent.rename(self.root / "Library/Fin")
            self.runtime = self.root / "Library/Fin/runtime"
            self.private = self.root / "Library/Fin/private"
            self.lock = self.root / "Library/Fin/.runtime-update.lock"
            run = self.runtime.parent / "run"
            run.mkdir()
            (run / "daemon-stopped").write_bytes(b"Fin broker stopped\n")
            previous_backup = self.runtime.parent / "runtime-backup.previous"
            previous_backup.mkdir()
            (previous_backup / "retained").write_bytes(b"previous reviewed runtime backup")
        for bundle in (self.old, self.runtime):
            if name == "fin":
                for component in FILES[:3]:
                    (bundle / component).rename(bundle / component.replace("gofin", "fin", 1))
                self.manifest(bundle, "fin")
        if name == "fin":
            (self.runtime / "authorization/GoFin").rename(self.runtime / "authorization/Fin")
        label = "io.fin-cli.broker" if namespace else "io.gofin-cli.broker"
        self.plist = self.root / ("Library/LaunchDaemons/" + label + ".plist")
        self.plist.parent.mkdir()
        self.plist.parent.chmod(0o755)
        self.plist.write_bytes(plistlib.dumps({
            "Label": label,
            "ProgramArguments": [str(self.runtime / (name + "-broker"))],
            "WorkingDirectory": str(self.runtime),
            "UserName": "root", "GroupName": "wheel",
            "Sockets": {"Broker": {"SockPathName": str(self.runtime.parent / "run/broker.sock"),
                                    "SockPathOwner": 501}},
        }))
        self.plist.chmod(0o644)
        self.previous_plist = self.plist.read_bytes()
        self.registration = self.root / ("private/etc/paths.d/" + ("fin" if namespace else "gofin"))
        self.registration.parent.mkdir(parents=True)
        for directory in (self.root / "private", self.root / "private/etc", self.registration.parent):
            directory.chmod(0o755)
        self.registration.write_text(str(self.runtime) + "\n")
        self.registration.chmod(0o644)

    def assert_legacy_restored(self):
        self.assertEqual(self.plist.read_bytes(), self.previous_plist)
        self.assertEqual(self.registration.read_text(), str(self.runtime) + "\n")
        for name in FILES:
            legacy = name.replace("gofin", "fin", 1)
            self.assertEqual((self.runtime / legacy).read_bytes(), (self.old / legacy).read_bytes())
        for name in FILES[:3]:
            self.assertFalse((self.runtime / name).exists())
        self.assertEqual((self.runtime / "authorization/Fin").read_bytes(), (self.old / "fin-touch-id").read_bytes())
        self.assertFalse((self.runtime / "authorization/GoFin").exists())
        self.assertFalse(self.target_runtime.parent.exists())
        self.assertFalse(self.target_plist.exists())
        self.assertFalse(self.target_registration.exists())

    def test_legacy_name_migration_preserves_private_state_and_reloads_service(self):
        self.prepare_legacy_layout()
        result = self.run_update(from_runtime=True, presentation_update=True)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assert_runtime_matches(self.new)
        for name in ("fin", "fin-broker", "fin-touch-id", "authorization/Fin"):
            self.assertFalse((self.runtime / name).exists())
        self.assertEqual(plistlib.loads(self.plist.read_bytes())["ProgramArguments"], [str(self.runtime / "gofin-broker")])
        self.assertEqual(self.registration.read_text(), str(self.runtime) + "\n")
        launchctl = [args for name, args in self.events() if name == "launchctl"]
        self.assertEqual(launchctl, [["print", "system/io.fin-cli.broker"], ["print", "system/io.gofin-cli.broker"], ["bootout", "system/io.fin-cli.broker"], ["bootstrap", "system", str(self.plist)]])
        self.assertFalse((self.root / "Library/Fin").exists())
        self.assertEqual((self.runtime.parent / "run/daemon-stopped").read_bytes(), b"Fin broker stopped\n")
        self.assertEqual((self.runtime.parent / "runtime-backup.previous/retained").read_bytes(), b"previous reviewed runtime backup")
        backup = next(path for path in self.runtime.parent.glob("runtime-backup.*") if (path / "previous.plist").exists())
        self.assertEqual((backup / "previous.plist").read_bytes(), self.previous_plist)
        self.assertEqual((backup / "previous/fin-broker").read_bytes(), (self.old / "fin-broker").read_bytes())

    def test_legacy_name_migration_requires_explicit_review_flag(self):
        self.prepare_legacy_layout()
        self.assert_refused("Renaming Fin requires the explicit")
        self.assert_legacy_restored()

    def test_legacy_name_migration_restores_names_and_plist_when_bootstrap_fails(self):
        self.prepare_legacy_layout()
        self.options["fail_bootstrap"] = True
        result = self.run_update(presentation_update=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Previous runtime restored", result.stderr)
        self.assert_legacy_restored()
        self.assertFalse(self.lock.exists())

    def test_legacy_name_migration_refuses_unknown_plist_without_replacement(self):
        self.prepare_legacy_layout()
        service = plistlib.loads(self.plist.read_bytes())
        service["ProgramArguments"].append("--unexpected")
        self.plist.write_bytes(plistlib.dumps(service))
        self.previous_plist = self.plist.read_bytes()
        result = self.run_update(presentation_update=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("registration is incompatible", result.stderr)
        self.assertFalse(any(command == "mv" for command, _ in self.events()))
        self.assert_legacy_restored()

    def test_legacy_name_migration_rolls_back_signal_after_plist_rename(self):
        self.prepare_legacy_layout()
        self.options["signal_activation"] = 7
        result = self.run_update(presentation_update=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Previous runtime restored", result.stderr)
        self.assert_legacy_restored()

    def test_legacy_name_migration_refuses_active_broker(self):
        self.prepare_legacy_layout()
        self.options["active_broker"] = True
        result = self.run_update(presentation_update=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Stop the installed daemon", result.stderr)
        self.assertFalse(any(command == "mv" for command, _ in self.events()))
        self.assert_legacy_restored()

    def test_legacy_name_migration_restores_after_partial_binary_replacement(self):
        self.prepare_legacy_layout()
        self.options["fail_activation"] = 2
        result = self.run_update(presentation_update=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Previous runtime restored", result.stderr)
        self.assert_legacy_restored()

    def test_firststage_gofin_installation_migrates_root_and_identities(self):
        self.prepare_legacy_layout(name="gofin")
        private_inode = self.private.stat().st_ino
        journal_inode = (self.private / "operations.sqlite").stat().st_ino
        result = self.run_update(presentation_update=True, from_runtime=True)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assert_runtime_matches(self.new)
        self.assertFalse((self.root / "Library/Fin").exists())
        self.assertEqual(self.private.stat().st_ino, private_inode)
        self.assertEqual((self.private / "operations.sqlite").stat().st_ino, journal_inode)
        service = plistlib.loads(self.plist.read_bytes())
        self.assertEqual(service["Label"], "io.gofin-cli.broker")
        self.assertEqual(service["WorkingDirectory"], str(self.runtime))
        self.assertEqual(service["Sockets"]["Broker"]["SockPathName"], str(self.runtime.parent / "run/broker.sock"))

    def test_namespace_migration_rolls_back_signal_immediately_after_root_move(self):
        self.prepare_legacy_layout()
        self.options["signal_root_move"] = True
        result = self.run_update(presentation_update=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Previous runtime restored", result.stderr)
        self.assert_legacy_restored()
        self.assertFalse(self.lock.exists())

    def test_namespace_migration_rolls_back_failed_root_move(self):
        self.prepare_legacy_layout()
        self.options["fail_root_move"] = True
        result = self.run_update(presentation_update=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Previous runtime restored", result.stderr)
        self.assert_legacy_restored()

    def test_namespace_migration_refuses_duplicate_root(self):
        self.prepare_legacy_layout()
        self.target_runtime.parent.mkdir()
        sentinel = self.target_runtime.parent / "retain"
        sentinel.write_bytes(b"another installation")
        result = self.run_update(presentation_update=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Both Fin and GoFin", result.stderr)
        self.assertEqual(sentinel.read_bytes(), b"another installation")
        self.assertFalse(any(command == "mv" for command, _ in self.events()))

    def test_namespace_migration_refuses_existing_target_registration(self):
        self.prepare_legacy_layout()
        self.target_registration.write_bytes(b"another registration\n")
        result = self.run_update(presentation_update=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("service or PATH registration already exists", result.stderr)
        self.assertEqual(self.target_registration.read_bytes(), b"another registration\n")
        self.assertFalse(any(command == "mv" for command, _ in self.events()))

    def test_namespace_migration_refuses_cross_filesystem_root_rename(self):
        self.prepare_legacy_layout()
        self.options["devices"] = {str(self.runtime.parent): "1", str(self.runtime): "1"}
        result = self.run_update(presentation_update=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("atomic same-filesystem rename", result.stderr)
        self.assert_legacy_restored()

    def test_current_namespace_refuses_stale_legacy_registration(self):
        registration = self.root / "private/etc/paths.d/fin"
        registration.parent.mkdir(parents=True)
        registration.write_bytes(b"legacy registration\n")
        result = self.run_update()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Legacy Fin registration remains", result.stderr)
        self.assertEqual(registration.read_bytes(), b"legacy registration\n")
        self.assertFalse(any(command == "mv" for command, _ in self.events()))

    def test_namespace_migration_refuses_busy_maintenance_lock(self):
        import fcntl
        self.prepare_legacy_layout()
        with (self.private / "maintenance.lock").open() as lock:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            result = self.run_update(presentation_update=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("maintenance lock is busy or invalid", result.stderr)
        self.assert_legacy_restored()

    def test_legacy_name_migration_does_not_overwrite_mixed_layout(self):
        self.prepare_legacy_layout()
        (self.runtime / "gofin").write_text("unowned entrypoint\n")
        result = self.run_update(presentation_update=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("fixed seven-component", result.stderr)
        self.assertEqual((self.runtime / "gofin").read_text(), "unowned entrypoint\n")
        self.assertFalse(any(command == "mv" for command, _ in self.events()))

    def install_fake_broker(self, path, label):
        code = r"""
import fcntl, json, os, pathlib, sqlite3, sys, time
root = pathlib.Path(ROOT)
options = json.loads((root / 'options.json').read_text())
if sys.argv[1:] == ['--export-storage-contract']:
    print(json.dumps({'journal_schema_version': int(os.environ.get('GOFIN_TEST_SCHEMA', '4'))}))
elif sys.argv[1:2] == ['--verify-update-bundle']:
    with (root / 'events.jsonl').open('a') as log:
        log.write(json.dumps(['verify-bundle', sys.argv[2:]]) + '\n')
    sys.exit(1 if options.get('bad_public_libraries') else 0)
elif sys.argv[1:2] == ['--hold-runtime-update']:
    if options.get('fail_public_compatibility'): sys.exit(1)
    lock = (root / 'Library/GoFin/private/maintenance.lock').open('rb')
    fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    with sqlite3.connect('file:' + str(root / 'Library/GoFin/private/operations.sqlite') + '?mode=ro&immutable=1', uri=True) as db:
        if db.execute('PRAGMA user_version').fetchone()[0] != int(os.environ.get('GOFIN_TEST_SCHEMA', '4')): sys.exit(1)
    db.close()
    print('compatible', flush=True)
    while os.getppid() == int(sys.argv[2]): time.sleep(0.05)
else: sys.exit(2)
"""
        path.write_text("#!" + sys.executable + "\n# synthetic " + label + " broker\n" + code.replace("ROOT", repr(str(self.root))))
        path.chmod(0o755)

    @staticmethod
    def manifest(bundle, name="gofin"):
        bundle.joinpath("SHA256SUMS").write_text("".join(
            hashlib.sha256(bundle.joinpath(name).read_bytes()).hexdigest() + "  " + name + "\n"
            for name in (name, name + "-broker", name + "-touch-id", *FILES[3:-1])))
        bundle.joinpath("SHA256SUMS").chmod(0o644)

    def run_update(self, sudo_uid="501", presentation_update=False, from_runtime=False, public_release=False):
        self.root.joinpath("options.json").write_text(json.dumps(self.options))
        arguments = ["/bin/bash", str(self.script), "--from", str(self.runtime if from_runtime else self.old), "--to", str(self.new)]
        if presentation_update:
            arguments.append("--presentation-update")
        if public_release:
            arguments.append("--public-release")
        result = subprocess.run(
            arguments,
            capture_output=True, text=True, timeout=30,
            env={"PATH": "/usr/bin:/bin:/usr/sbin:/sbin", "SUDO_UID": sudo_uid,
                 "GOFIN_TEST_SCHEMA": str(self.options.get("supported_schema", 4))})
        if result.returncode == 0 and hasattr(self, "target_runtime") and self.target_runtime.exists():
            self.runtime = self.target_runtime
            self.private = self.runtime.parent / "private"
            self.lock = self.runtime.parent / ".runtime-update.lock"
            self.plist = self.target_plist
            self.registration = self.target_registration
        self.assertEqual(self.private_before, {str(path.relative_to(self.private)): path.read_bytes()
                                              for path in self.private.rglob("*") if path.is_file()})
        if hasattr(self, "run_before"):
            self.assertEqual(self.run_before, {path.name: path.read_bytes() for path in (self.runtime.parent / "run").iterdir() if path.is_file()})
        for command, args in self.events():
            self.assertNotIn(command, ("gofin", "gofin-broker", "gofin-touch-id", "fin", "fin-broker", "fin-touch-id"))
            for arg in args:
                state_path = any(arg.endswith("/" + product + "/" + part) or "/" + product + "/" + part + "/" in arg
                                 for product in ("Fin", "GoFin") for part in ("private", "run"))
                allowed_lock = command in ("stat", "ls", "python3") and arg.endswith(("/private", "/private/maintenance.lock"))
                self.assertFalse(state_path and not allowed_lock)
        return result

    def events(self):
        path = self.root / "events.jsonl"
        return [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []

    def assert_runtime_matches(self, bundle):
        for name in FILES:
            self.assertEqual(self.runtime.joinpath(name).read_bytes(), bundle.joinpath(name).read_bytes(), name)
            expected_mode = 0o755 if name in FILES[:4] else 0o644
            self.assertEqual(stat.S_IMODE(self.runtime.joinpath(name).stat().st_mode), expected_mode, name)
        helper_dir = self.runtime / "authorization"
        helper = helper_dir / "GoFin"
        if helper_dir.exists():
            self.assertEqual(helper.read_bytes(), bundle.joinpath("gofin-touch-id").read_bytes())
            self.assertEqual(stat.S_IMODE(helper_dir.stat().st_mode), 0o755)
            self.assertEqual(stat.S_IMODE(helper.stat().st_mode), 0o755)

    def assert_refused(self, expected):
        result = self.run_update()
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn(expected, result.stderr)
        self.assertFalse(any(command == "mv" for command, _ in self.events()))
        self.assertNotIn("GoFin updated.", result.stdout)
        return result

    def test_applies_atomic_changed_files_and_preserves_exact_runtime_backup(self):
        result = self.run_update()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assert_runtime_matches(self.new)
        backups = list(self.runtime.parent.glob("runtime-backup.*"))
        self.assertEqual(len(backups), 1)
        backup = backups[0]
        self.assertEqual({path.name for path in backup.iterdir()}, {"previous", "previous-GoFin"})
        self.assertEqual(stat.S_IMODE(backup.stat().st_mode), 0o700)
        for name in FILES:
            self.assertEqual(backup.joinpath("previous", name).read_bytes(), self.old.joinpath(name).read_bytes())
        self.assertEqual(backup.joinpath("previous-GoFin").read_bytes(), self.old.joinpath("gofin-touch-id").read_bytes())
        renamed = [Path(args[-1]).name for command, args in self.events() if command == "mv"]
        self.assertEqual(renamed, ["gofin", "gofin-broker", "BUILDINFO", "SHA256SUMS"])
        self.assertFalse(self.lock.exists())
        self.assertEqual(result.stdout, "Updating GoFin...\n✓ GoFin updated.\n")

    def test_newer_journal_schema_refuses_before_any_runtime_replacement(self):
        with sqlite3.connect(self.private / "operations.sqlite") as connection:
            connection.execute("PRAGMA user_version=9")
        connection.close()
        self.private_before = {str(path.relative_to(self.private)): path.read_bytes()
                               for path in self.private.rglob("*") if path.is_file()}
        result = self.assert_refused("newer than incoming broker schema")
        self.assert_runtime_matches(self.old)
        self.assertNotIn("restoring changed runtime files", result.stderr)

    def test_matching_newer_schema_allows_update(self):
        with sqlite3.connect(self.private / "operations.sqlite") as connection:
            connection.execute("PRAGMA user_version=9")
        connection.close()
        self.private_before = {str(path.relative_to(self.private)): path.read_bytes()
                               for path in self.private.rglob("*") if path.is_file()}
        self.options["supported_schema"] = 9
        result = self.run_update()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assert_runtime_matches(self.new)

    def test_installed_runtime_can_supply_verified_old_bundle_without_private_access(self):
        self.new.joinpath("gofin-touch-id").write_text("synthetic new presentation helper\n")
        self.manifest(self.new)
        result = self.run_update(presentation_update=True, from_runtime=True)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assert_runtime_matches(self.new)
        self.assertFalse(self.lock.exists())

    def prepare_public_update(self, *, running=False):
        self.prepare_legacy_layout(name="gofin", namespace=False)
        self.options["public_running"] = running
        run = self.runtime.parent / "run"
        run.mkdir(exist_ok=True)
        if not running:
            (run / "daemon-stopped").write_bytes(b"synthetic persistent human stop intent\n")
        (run / "owned-sentinel").write_bytes(b"synthetic runtime socket-directory sentinel\x00")
        self.run_before = {path.name: path.read_bytes() for path in run.iterdir() if path.is_file()}

    def test_public_update_preserves_all_private_bytes_and_running_service(self):
        self.prepare_public_update(running=True)
        result = self.run_update(from_runtime=True, public_release=True)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assert_runtime_matches(self.new)
        self.assertFalse(any(name in ("python3", "otool") for name, _ in self.events()))
        events = self.events()
        verify = next(i for i, (name, _) in enumerate(events) if name == "verify-bundle")
        detach = next(i for i, (name, args) in enumerate(events) if name == "launchctl" and args[0] == "bootout")
        self.assertLess(verify, detach)
        lifecycle = [args[0] for name, args in events if name == "launchctl"]
        self.assertEqual(lifecycle, ["print", "bootout", "bootstrap", "kickstart", "print"])

    def test_public_update_keeps_stopped_service_stopped(self):
        self.prepare_public_update()
        result = self.run_update(from_runtime=True, public_release=True)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertFalse(any(name == "launchctl" and args[0] == "kickstart" for name, args in self.events()))

    def test_public_update_partial_failure_restores_runtime_and_prior_service(self):
        self.prepare_public_update(running=True)
        self.options["fail_activation"] = 2
        result = self.run_update(from_runtime=True, public_release=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Previous runtime restored", result.stderr)
        self.assert_runtime_matches(self.old)
        lifecycle = [args[0] for name, args in self.events() if name == "launchctl"]
        self.assertEqual(lifecycle, ["print", "bootout", "bootout", "bootstrap", "kickstart"])

    def test_public_update_signal_restores_runtime_and_prior_service(self):
        self.prepare_public_update(running=True)
        self.options["signal_activation"] = 2
        result = self.run_update(from_runtime=True, public_release=True)
        self.assertNotEqual(result.returncode, 0)
        self.assert_runtime_matches(self.old)
        self.assertIn("Previous runtime restored", result.stderr)

    def test_public_update_rejected_schema_does_not_stop_service(self):
        self.prepare_public_update(running=True)
        self.options["supported_schema"] = 5
        result = self.run_update(from_runtime=True, public_release=True)
        self.assertNotEqual(result.returncode, 0)
        self.assert_runtime_matches(self.old)
        self.assertFalse(any(name == "launchctl" and args[0] == "bootout" for name, args in self.events()))

    def test_public_update_rejected_libraries_do_not_execute_incoming_or_stop_service(self):
        self.prepare_public_update(running=True)
        self.options["bad_public_libraries"] = True
        result = self.run_update(from_runtime=True, public_release=True)
        self.assertNotEqual(result.returncode, 0)
        self.assert_runtime_matches(self.old)
        self.assertIn("library contract", result.stderr)
        self.assertFalse(any(name == "launchctl" and args[0] == "bootout" for name, args in self.events()))

    def test_public_update_unobserved_restart_rolls_back_after_bounded_wait(self):
        self.prepare_public_update(running=True)
        self.options["missing_public_restart"] = True
        # Advance the wait bound only in this disposable rewritten fixture.
        source = self.script.read_text()
        self.assertEqual(source.count("$((SECONDS + 60))"), 1)
        self.script.write_text(source.replace("$((SECONDS + 60))", "$((SECONDS + 0))"))
        result = self.run_update(from_runtime=True, public_release=True)
        self.assertNotEqual(result.returncode, 0)
        self.assert_runtime_matches(self.old)
        self.assertIn("The updated service did not start", result.stderr)
        self.assertIn("Previous runtime restored", result.stderr)

    def test_public_update_restart_failure_restores_runtime_and_prior_service(self):
        self.prepare_public_update(running=True)
        self.options["fail_public_kickstart_once"] = True
        result = self.run_update(from_runtime=True, public_release=True)
        self.assertNotEqual(result.returncode, 0)
        self.assert_runtime_matches(self.old)
        self.assertIn("Previous runtime restored", result.stderr)
        lifecycle = [args[0] for name, args in self.events() if name == "launchctl"]
        self.assertEqual(lifecycle, ["print", "bootout", "bootstrap", "kickstart", "bootout", "bootstrap", "kickstart"])

    def test_public_update_bootstrap_failure_rolls_back_and_restores_registration(self):
        self.prepare_public_update(running=True)
        self.options["fail_public_bootstrap_once"] = True
        result = self.run_update(from_runtime=True, public_release=True)
        self.assertNotEqual(result.returncode, 0)
        self.assert_runtime_matches(self.old)
        self.assertIn("Previous runtime restored", result.stderr)

    def test_requires_human_sudo_and_root(self):
        self.options["uid"] = "501"
        self.assert_refused("fresh sudo -k")
        self.options["uid"] = "0"
        result = self.run_update(sudo_uid="")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("human sudo", result.stderr)

    def test_shipped_script_refuses_missing_terminal(self):
        # Restore just the real TTY guard while retaining fixture-only paths.
        self.script.write_text(self.script.read_text().replace(
            "true || fail 'Use a fresh human Terminal", "[ -t 0 ] && [ -t 1 ] && [ -t 2 ] || fail 'Use a fresh human Terminal"))
        self.assert_refused("without pipelines or redirection")

    def test_make_update_can_use_verified_runtime_as_old_bundle(self):
        result = self.run_update(from_runtime=True, presentation_update=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assert_runtime_matches(self.new)

    def test_skill_refresh_drops_privileges_after_runtime_commit(self):
        result = self.run_update()
        self.assertEqual(result.returncode, 0, result.stderr)
        events = self.events()
        refresh = [event for event in events if event[0] == "sudo" and event[1][-1] == "internal-skills-refresh"]
        self.assertEqual(len(refresh), 1)
        self.assertEqual(refresh[0][1], ["-n", "-H", "-u", "#501", str(self.bin / "env"), "-i", str(self.runtime / "gofin"), "internal-skills-refresh"])
        self.assertGreater(events.index(refresh[0]), max(i for i, event in enumerate(events) if event[0] == "mv"))

    def test_completion_refresh_drops_privileges_after_runtime_commit(self):
        result = self.run_update()
        self.assertEqual(result.returncode, 0, result.stderr)
        events = self.events()
        refresh = [event for event in events if event[0] == "sudo" and event[1][-2:] == ["internal-completions", "refresh"]]
        self.assertEqual(len(refresh), 1)
        self.assertEqual(refresh[0][1], ["-n", "-H", "-u", "#501", str(self.bin / "env"), "-i", str(self.runtime / "gofin"), "internal-completions", "refresh"])
        self.assertGreater(events.index(refresh[0]), max(i for i, event in enumerate(events) if event[0] == "mv"))

    def test_completion_refresh_failure_preserves_successful_runtime_update(self):
        self.options["completion_refresh_failure"] = True
        result = self.run_update()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("optional shell completion refresh needs attention", result.stderr)
        self.assert_runtime_matches(self.new)
        self.assertIn("GoFin updated.", result.stdout)

    def test_skill_refresh_failure_preserves_successful_runtime_update(self):
        self.options["skill_refresh_failure"] = True
        result = self.run_update()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("optional skill refresh needs attention", result.stderr)
        self.assertEqual((self.runtime / "gofin").read_bytes(), (self.new / "gofin").read_bytes())

    def test_installed_hash_mismatch_is_preserved(self):
        self.runtime.joinpath("gofin-broker").write_text("unexpected installed bytes\n")
        self.assert_refused("checksum verification failed")
        self.assertEqual(self.runtime.joinpath("gofin-broker").read_text(), "unexpected installed bytes\n")

    def test_valid_but_different_installed_bundle_is_refused(self):
        self.runtime.joinpath("README.md").write_text("unexpected installed docs\n")
        self.manifest(self.runtime)
        self.assert_refused("does not exactly match --from")

    def test_tampered_new_bundle_is_refused(self):
        self.new.joinpath("gofin-broker").write_text("tampered fixture\n")
        self.assert_refused("checksum verification failed")
        self.assert_runtime_matches(self.old)

    def test_duplicate_manifest_entry_is_refused(self):
        manifest = self.new / "SHA256SUMS"
        manifest.write_text(manifest.read_text() + manifest.read_text().splitlines()[0] + "\n")
        self.assert_refused("manifest is invalid")

    def test_manifest_path_traversal_is_refused(self):
        manifest = self.new / "SHA256SUMS"
        manifest.write_text(manifest.read_text().replace("  gofin\n", "  ../private\n"))
        self.assert_refused("manifest is invalid")

    def test_helper_and_installer_changes_need_explicit_presentation_flag(self):
        for name in ("gofin-touch-id", "install.sh"):
            with self.subTest(component=name):
                original = self.new.joinpath(name).read_bytes()
                self.new.joinpath(name).write_text("changed fixture\n")
                self.manifest(self.new)
                self.assert_refused("explicit --presentation-update flag")
                self.new.joinpath(name).write_bytes(original)
                self.manifest(self.new)

    def test_presentation_update_explicitly_allows_helper_and_installer(self):
        self.new.joinpath("gofin-touch-id").write_text("reviewed presentation helper\n")
        self.new.joinpath("install.sh").write_text("reviewed concise installer\n")
        self.manifest(self.new)
        result = self.run_update(presentation_update=True)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assert_runtime_matches(self.new)
        renamed = [Path(args[-1]).name for command, args in self.events() if command == "mv"]
        self.assertEqual(renamed, ["gofin", "gofin-broker", "gofin-touch-id", "install.sh", "BUILDINFO", "SHA256SUMS", "GoFin"])

    def test_legacy_runtime_gains_derived_fin_helper(self):
        shutil.rmtree(self.runtime / "authorization")
        result = self.run_update()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assert_runtime_matches(self.new)
        self.assertIn("GoFin", [Path(args[-1]).name for command, args in self.events() if command == "mv"])

    def test_existing_derived_helper_mismatch_is_refused(self):
        helper = self.runtime / "authorization/GoFin"
        helper.write_text("tampered synthetic alias\n")
        self.assert_refused("differs from its signed source component")
        self.assertEqual(helper.read_text(), "tampered synthetic alias\n")

    def test_derived_helper_is_restored_when_post_update_verification_fails(self):
        old_helper = (self.runtime / "authorization/GoFin").read_bytes()
        self.new.joinpath("gofin-touch-id").write_text("reviewed synthetic helper\n")
        self.new.joinpath("install.sh").write_text("reviewed synthetic installer\n")
        self.manifest(self.new)
        self.options["post_replace_signature_failure"] = True
        result = self.run_update(presentation_update=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Previous runtime restored", result.stderr)
        self.assertEqual((self.runtime / "authorization/GoFin").read_bytes(), old_helper)
        self.assert_runtime_matches(self.old)

    def test_wrong_architecture_is_refused(self):
        self.new.joinpath("BUILDINFO").write_text("architecture=x86_64\n")
        self.manifest(self.new)
        self.assert_refused("architecture does not match")

    def test_helper_substitution_after_preflight_is_refused_from_staged_bytes(self):
        self.options["race_helper"] = True
        self.assert_refused("Staged helper/installer changed")
        self.assert_runtime_matches(self.old)

    def test_symlink_substitution_is_refused_before_recursive_metadata_changes(self):
        self.options["race_symlink"] = True
        self.assert_refused("symbolic-link component")
        self.assertFalse(any(command == "chown" for command, _ in self.events()))
        self.assert_runtime_matches(self.old)

    def test_symlink_source_is_refused(self):
        self.new.joinpath("gofin").unlink()
        self.new.joinpath("gofin").symlink_to(self.old / "gofin")
        self.assert_refused("symbolic-link component")

    def test_symlink_installed_directory_is_refused(self):
        actual = self.runtime.with_name("renamed-runtime")
        self.runtime.rename(actual)
        self.runtime.symlink_to(actual, target_is_directory=True)
        self.assert_refused("real and root-owned")

    def test_unsafe_ownership_permissions_and_acl_are_refused(self):
        path = str(self.runtime)
        self.options["owners"] = {path: 501}
        self.assert_refused("real and root-owned")
        self.options["owners"] = {}
        self.runtime.chmod(0o775)
        self.assert_refused("no group/world write access")
        self.runtime.chmod(0o755)
        self.options["acls"] = [path]
        self.assert_refused("extended ACLs")

    def test_extra_runtime_component_is_refused(self):
        self.runtime.joinpath("unexpected").write_text("retain\n")
        self.assert_refused("fixed seven-component")
        self.assertEqual(self.runtime.joinpath("unexpected").read_text(), "retain\n")

    def test_cross_filesystem_runtime_refuses_nonatomic_copy_fallback(self):
        self.options["devices"] = {str(self.runtime): "1"}
        self.assert_refused("must share a filesystem")

    def test_signature_hardened_runtime_and_os_library_checks(self):
        for option, expected in (("bad_signature", "signature verification failed"),
                                 ("no_hardened", "lacks hardened signing"),
                                 ("non_os_library", "non-OS libraries")):
            with self.subTest(failure=option):
                self.options = {option: True}
                self.assert_refused(expected)
                self.assert_runtime_matches(self.old)
                self.assertFalse(list(self.runtime.parent.glob("runtime-backup.*")))

    def test_active_clients_enrollment_and_process_inspection_errors_refuse(self):
        for option in ("active_client", "active_enrollment", "pgrep_error", "late_enrollment"):
            with self.subTest(activity=option):
                self.options = {option: True}
                self.assert_refused("active or process inspection failed")
                self.assert_runtime_matches(self.old)

    def test_concurrent_or_interrupted_updater_lock_is_preserved(self):
        self.lock.mkdir()
        self.assert_refused("Another runtime update or an interrupted update")
        self.assertTrue(self.lock.is_dir())

    def test_partial_file_failure_restores_old_runtime_and_modes(self):
        self.options["fail_activation"] = 2
        result = self.run_update()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Previous runtime restored", result.stderr)
        self.assertNotIn("GoFin updated.", result.stdout)
        self.assert_runtime_matches(self.old)
        self.assertEqual(len(list(self.runtime.parent.glob("runtime-backup.*/previous"))), 1)
        self.assertFalse(self.lock.exists())

    def test_signal_between_rename_and_next_command_restores_runtime(self):
        self.options["signal_activation"] = 1
        result = self.run_update()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Previous runtime restored", result.stderr)
        self.assert_runtime_matches(self.old)

    def test_post_replacement_verification_failure_restores_old_runtime(self):
        self.options["post_replace_signature_failure"] = True
        result = self.run_update()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Previous runtime restored", result.stderr)
        self.assertNotIn("GoFin updated.", result.stdout)
        self.assert_runtime_matches(self.old)

    def test_failed_rollback_keeps_evidence_and_reports_uncertain(self):
        self.options.update(fail_activation=2, fail_restore=True)
        result = self.run_update()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Runtime restoration is uncertain", result.stderr)
        self.assertNotIn("Previous runtime restored", result.stderr)
        self.assertEqual(self.runtime.joinpath("gofin").read_bytes(), self.new.joinpath("gofin").read_bytes())
        backup = next(self.runtime.parent.glob("runtime-backup.*/previous"))
        for name in FILES:
            self.assertEqual(backup.joinpath(name).read_bytes(), self.old.joinpath(name).read_bytes())


if __name__ == "__main__":
    unittest.main(verbosity=2)
