#!/usr/bin/env python3
"""Runtime-hotfix regression fixtures; no privilege, real state, or Fin execution.

Run: python3 scripts/test-rust-macos-runtime-update.py
The harness rewrites fixed system paths into one temporary tree and replaces OS,
privilege, process, and signature checks. It does not establish real root/Touch ID
behavior or make the advisory process checks a synchronization guarantee.
"""

import hashlib
import json
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
FILES = ("fin", "fin-broker", "fin-touch-id", "install.sh", "README.md", "BUILDINFO", "SHA256SUMS")
DRIVER = r'''
import hashlib, json, os, pathlib, signal, stat, sys
root = pathlib.Path(ROOT)
name = pathlib.Path(sys.argv[0]).name
args = sys.argv[1:]
options = json.loads((root / "options.json").read_text())
events_file = root / "events.jsonl"
events = [json.loads(line) for line in events_file.read_text().splitlines()] if events_file.exists() else []
with events_file.open("a") as log:
    log.write(json.dumps([name, args]) + "\n")
# A rewriting mistake must fail before any real system or private-state access.
for argument in (args[1:] if name == "awk" and not args[0].startswith("-") else args):
    if argument.startswith("/"):
        if not argument.startswith(str(root) + "/"):
            sys.exit("Refusing non-fixture path: " + argument)
        if any(argument.endswith("/Fin/" + part) or "/Fin/" + part + "/" in argument for part in ("private", "run")):
            sys.exit("Refusing access to private state or service socket: " + argument)
if name in ("mkdir", "rmdir", "cp", "rm", "mv", "mktemp", "chmod"):
    if name == "chmod" and args[0] == "-RN":
        sys.exit(0)
    if name == "cp" and args[-2] == str(root / "new bundle/fin") and options.get("race_symlink"):
        broker = root / "new bundle/fin-broker"
        broker.unlink()
        broker.symlink_to(root / "old bundle/fin-broker")
    if name == "cp" and args[-2] == str(root / "new bundle/fin") and options.get("race_helper"):
        bundle = root / "new bundle"
        bundle.joinpath("fin-touch-id").write_text("synthetic raced helper\n")
        bundle.joinpath("SHA256SUMS").write_text("".join(
            hashlib.sha256(bundle.joinpath(component).read_bytes()).hexdigest() + "  " + component + "\n"
            for component in ("fin", "fin-broker", "fin-touch-id", "install.sh", "README.md", "BUILDINFO")))
    if name == "mv":
        activation = "/incoming/" in args[-2] or pathlib.Path(args[-2]).name == "incoming-Fin"
        activation_count = 1 + sum(
            event[0] == "mv" and ("/incoming/" in event[1][-2] or pathlib.Path(event[1][-2]).name == "incoming-Fin")
            for event in events)
        if activation and options.get("fail_activation") == activation_count:
            sys.exit(1)
        if ("/restore/" in args[-2] or pathlib.Path(args[-2]).name == "restore-Fin") and options.get("fail_restore"):
            sys.exit(1)
        if activation and options.get("signal_activation") == activation_count:
            os.replace(args[-2], args[-1])
            os.kill(os.getppid(), signal.SIGTERM)
            sys.exit(0)
    path = ("/usr/bin/" if name == "mktemp" else "/bin/") + name
    os.execv(path, [name] + args)
elif name in ("awk", "shasum", "cmp", "sudo"):
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
            args[-1].startswith(str(root / "Library/Fin/runtime") + "/") and
            any(event[0] == "mv" for event in events)):
        sys.exit(1)
    if "/incoming/" in args[-1] or pathlib.Path(args[-1]).name == "incoming-Fin":
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
    # A persistent broker daemon by itself is deliberately allowed.
    sys.exit(1)
elif name == "sudo":
    sys.exit(1 if options.get("skill_refresh_failure") else 0)
elif name != "chown":
    sys.exit("Unexpected mock command: " + name)
'''


class RuntimeUpdateFixture(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="fin-runtime-update-test-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name).resolve()
        self.old = self.root / "old bundle"
        self.new = self.root / "new bundle"
        self.runtime = self.root / "Library/Fin/runtime"
        self.private = self.root / "Library/Fin/private"
        self.lock = self.root / "Library/Fin/.runtime-update.lock"
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
        shutil.copy2(self.runtime / "fin-touch-id", authorization / "Fin")
        authorization.chmod(0o755)
        (authorization / "Fin").chmod(0o755)
        self.new.joinpath("fin").write_text("synthetic new discovery\n")
        self.new.joinpath("fin-broker").write_text("synthetic new enrollment\n")
        self.install_fake_broker(self.old / "fin-broker", "old")
        self.install_fake_broker(self.new / "fin-broker", "new")
        shutil.copy2(self.old / "fin-broker", self.runtime / "fin-broker")
        self.manifest(self.old)
        self.manifest(self.runtime)
        self.new.joinpath("BUILDINFO").write_text("source_commit=synthetic-patch\nsource_state=modified\narchitecture=arm64\n")
        self.manifest(self.new)
        self.private.joinpath("config.json").write_bytes(b"synthetic private config\x00")
        with sqlite3.connect(self.private / "operations.sqlite") as connection:
            connection.execute("PRAGMA user_version=4")
            connection.execute("CREATE TABLE synthetic_unresolved_record (id TEXT)")
        self.private.joinpath("credentials").mkdir()
        self.private.joinpath("credentials/synthetic-only").write_bytes(b"synthetic credential\x00")
        self.private_before = {str(path.relative_to(self.private)): path.read_bytes()
                               for path in self.private.rglob("*") if path.is_file()}
        driver = "#!" + sys.executable + "\n" + DRIVER.replace("ROOT", repr(str(self.root)))
        for name in ("uname", "id", "stat", "ls", "codesign", "otool", "pgrep", "chown", "python3", "dirname",
                     "mkdir", "rmdir", "cp", "rm", "mv", "mktemp", "chmod", "awk", "shasum", "cmp", "sudo"):
            self.bin.joinpath(name).write_text(driver)
            self.bin.joinpath(name).chmod(0o755)
        source = SCRIPT.read_text()
        terminal_guard = '[ -t 0 ] && [ -t 1 ] && [ -t 2 ]'
        self.assertEqual(source.count(terminal_guard), 1)
        # There is no production fixture flag/environment override.
        source = source.replace(terminal_guard, "true")
        source = re.sub(r'(?<![\w/])/Library(?=[/\s";:]|$)',
                        lambda match: str(self.root) + match.group(0), source)
        source = re.sub(r'(?<![\w/])/(?:usr/)?s?bin/([a-z]+)',
                        lambda match: str(self.bin / match.group(1)), source)
        self.script = self.root / "update.sh"
        self.script.write_text(source)
        checker = SCRIPT.with_name("check-runtime-store-compatibility.py").read_text()
        checker = re.sub(r'(?<![\w/])/Library(?=[/\s";:]|$)',
                         lambda match: str(self.root) + match.group(0), checker)
        checker = checker.replace("if os.geteuid() != 0:", "if False:")
        checker = checker.replace("expected_uid=0)", "expected_uid=os.geteuid())")
        self.root.joinpath("check-runtime-store-compatibility.py").write_text(checker)

    def install_fake_broker(self, path, label):
        path.write_text("#!" + sys.executable + "\n# synthetic " + label + " broker\nimport json, os, sys\n"
                        "if sys.argv[1:] == ['--export-storage-contract']:\n"
                        " print(json.dumps({'journal_schema_version': int(os.environ.get('FIN_TEST_SCHEMA', '4'))}))\n"
                        "else: sys.exit(2)\n")
        path.chmod(0o755)

    @staticmethod
    def manifest(bundle):
        bundle.joinpath("SHA256SUMS").write_text("".join(
            hashlib.sha256(bundle.joinpath(name).read_bytes()).hexdigest() + "  " + name + "\n"
            for name in FILES[:-1]))
        bundle.joinpath("SHA256SUMS").chmod(0o644)

    def run_update(self, sudo_uid="501", presentation_update=False, from_runtime=False):
        self.root.joinpath("options.json").write_text(json.dumps(self.options))
        arguments = ["/bin/bash", str(self.script), "--from", str(self.runtime if from_runtime else self.old), "--to", str(self.new)]
        if presentation_update:
            arguments.append("--presentation-update")
        result = subprocess.run(
            arguments,
            capture_output=True, text=True, timeout=30,
            env={"PATH": "/usr/bin:/bin:/usr/sbin:/sbin", "SUDO_UID": sudo_uid,
                 "FIN_TEST_SCHEMA": str(self.options.get("supported_schema", 4))})
        self.assertEqual(self.private_before, {str(path.relative_to(self.private)): path.read_bytes()
                                              for path in self.private.rglob("*") if path.is_file()})
        for command, args in self.events():
            self.assertNotIn(command, ("launchctl", "fin", "fin-broker", "fin-touch-id"))
            self.assertFalse(any(arg.endswith("/Fin/" + part) or "/Fin/" + part + "/" in arg
                                 for arg in args for part in ("private", "run")))
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
        helper = helper_dir / "Fin"
        if helper_dir.exists():
            self.assertEqual(helper.read_bytes(), bundle.joinpath("fin-touch-id").read_bytes())
            self.assertEqual(stat.S_IMODE(helper_dir.stat().st_mode), 0o755)
            self.assertEqual(stat.S_IMODE(helper.stat().st_mode), 0o755)

    def assert_refused(self, expected):
        result = self.run_update()
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn(expected, result.stderr)
        self.assertFalse(any(command == "mv" for command, _ in self.events()))
        self.assertNotIn("Fin updated.", result.stdout)
        return result

    def test_applies_atomic_changed_files_and_preserves_exact_runtime_backup(self):
        result = self.run_update()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assert_runtime_matches(self.new)
        backups = list(self.runtime.parent.glob("runtime-backup.*"))
        self.assertEqual(len(backups), 1)
        backup = backups[0]
        self.assertEqual({path.name for path in backup.iterdir()}, {"previous", "previous-Fin"})
        self.assertEqual(stat.S_IMODE(backup.stat().st_mode), 0o700)
        for name in FILES:
            self.assertEqual(backup.joinpath("previous", name).read_bytes(), self.old.joinpath(name).read_bytes())
        self.assertEqual(backup.joinpath("previous-Fin").read_bytes(), self.old.joinpath("fin-touch-id").read_bytes())
        renamed = [Path(args[-1]).name for command, args in self.events() if command == "mv"]
        self.assertEqual(renamed, ["fin", "fin-broker", "BUILDINFO", "SHA256SUMS"])
        self.assertFalse(self.lock.exists())
        self.assertEqual(result.stdout, "Updating Fin...\n✓ Fin updated.\n")

    def test_newer_journal_schema_refuses_before_any_runtime_replacement(self):
        with sqlite3.connect(self.private / "operations.sqlite") as connection:
            connection.execute("PRAGMA user_version=9")
        self.private_before = {str(path.relative_to(self.private)): path.read_bytes()
                               for path in self.private.rglob("*") if path.is_file()}
        result = self.assert_refused("newer than incoming broker schema")
        self.assert_runtime_matches(self.old)
        self.assertNotIn("restoring changed runtime files", result.stderr)

    def test_matching_newer_schema_allows_update(self):
        with sqlite3.connect(self.private / "operations.sqlite") as connection:
            connection.execute("PRAGMA user_version=9")
        self.private_before = {str(path.relative_to(self.private)): path.read_bytes()
                               for path in self.private.rglob("*") if path.is_file()}
        self.options["supported_schema"] = 9
        result = self.run_update()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assert_runtime_matches(self.new)

    def test_installed_runtime_can_supply_verified_old_bundle_without_private_access(self):
        self.new.joinpath("fin-touch-id").write_text("synthetic new presentation helper\n")
        self.manifest(self.new)
        result = self.run_update(presentation_update=True, from_runtime=True)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assert_runtime_matches(self.new)
        self.assertFalse(self.lock.exists())

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
        refresh = [event for event in events if event[0] == "sudo"]
        self.assertEqual(len(refresh), 1)
        self.assertEqual(refresh[0][1], ["-n", "-H", "-u", "#501", str(self.bin / "env"), "-i", str(self.runtime / "fin"), "internal-skills-refresh"])
        self.assertGreater(events.index(refresh[0]), max(i for i, event in enumerate(events) if event[0] == "mv"))

    def test_skill_refresh_failure_preserves_successful_runtime_update(self):
        self.options["skill_refresh_failure"] = True
        result = self.run_update()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("optional skill refresh needs attention", result.stderr)
        self.assertEqual((self.runtime / "fin").read_bytes(), (self.new / "fin").read_bytes())

    def test_installed_hash_mismatch_is_preserved(self):
        self.runtime.joinpath("fin-broker").write_text("unexpected installed bytes\n")
        self.assert_refused("checksum verification failed")
        self.assertEqual(self.runtime.joinpath("fin-broker").read_text(), "unexpected installed bytes\n")

    def test_valid_but_different_installed_bundle_is_refused(self):
        self.runtime.joinpath("README.md").write_text("unexpected installed docs\n")
        self.manifest(self.runtime)
        self.assert_refused("does not exactly match --from")

    def test_tampered_new_bundle_is_refused(self):
        self.new.joinpath("fin-broker").write_text("tampered fixture\n")
        self.assert_refused("checksum verification failed")
        self.assert_runtime_matches(self.old)

    def test_duplicate_manifest_entry_is_refused(self):
        manifest = self.new / "SHA256SUMS"
        manifest.write_text(manifest.read_text() + manifest.read_text().splitlines()[0] + "\n")
        self.assert_refused("manifest is invalid")

    def test_manifest_path_traversal_is_refused(self):
        manifest = self.new / "SHA256SUMS"
        manifest.write_text(manifest.read_text().replace("  fin\n", "  ../private\n"))
        self.assert_refused("manifest is invalid")

    def test_helper_and_installer_changes_need_explicit_presentation_flag(self):
        for name in ("fin-touch-id", "install.sh"):
            with self.subTest(component=name):
                original = self.new.joinpath(name).read_bytes()
                self.new.joinpath(name).write_text("changed fixture\n")
                self.manifest(self.new)
                self.assert_refused("explicit --presentation-update flag")
                self.new.joinpath(name).write_bytes(original)
                self.manifest(self.new)

    def test_presentation_update_explicitly_allows_helper_and_installer(self):
        self.new.joinpath("fin-touch-id").write_text("reviewed presentation helper\n")
        self.new.joinpath("install.sh").write_text("reviewed concise installer\n")
        self.manifest(self.new)
        result = self.run_update(presentation_update=True)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assert_runtime_matches(self.new)
        renamed = [Path(args[-1]).name for command, args in self.events() if command == "mv"]
        self.assertEqual(renamed, ["fin", "fin-broker", "fin-touch-id", "install.sh", "BUILDINFO", "SHA256SUMS", "Fin"])

    def test_legacy_runtime_gains_derived_fin_helper(self):
        shutil.rmtree(self.runtime / "authorization")
        result = self.run_update()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assert_runtime_matches(self.new)
        self.assertIn("Fin", [Path(args[-1]).name for command, args in self.events() if command == "mv"])

    def test_existing_derived_helper_mismatch_is_refused(self):
        helper = self.runtime / "authorization/Fin"
        helper.write_text("tampered synthetic alias\n")
        self.assert_refused("differs from its signed source component")
        self.assertEqual(helper.read_text(), "tampered synthetic alias\n")

    def test_derived_helper_is_restored_when_post_update_verification_fails(self):
        old_helper = (self.runtime / "authorization/Fin").read_bytes()
        self.new.joinpath("fin-touch-id").write_text("reviewed synthetic helper\n")
        self.new.joinpath("install.sh").write_text("reviewed synthetic installer\n")
        self.manifest(self.new)
        self.options["post_replace_signature_failure"] = True
        result = self.run_update(presentation_update=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Previous runtime restored", result.stderr)
        self.assertEqual((self.runtime / "authorization/Fin").read_bytes(), old_helper)
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
        self.new.joinpath("fin").unlink()
        self.new.joinpath("fin").symlink_to(self.old / "fin")
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
        self.assertNotIn("Fin updated.", result.stdout)
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
        self.assertNotIn("Fin updated.", result.stdout)
        self.assert_runtime_matches(self.old)

    def test_failed_rollback_keeps_evidence_and_reports_uncertain(self):
        self.options.update(fail_activation=2, fail_restore=True)
        result = self.run_update()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Runtime restoration is uncertain", result.stderr)
        self.assertNotIn("Previous runtime restored", result.stderr)
        self.assertEqual(self.runtime.joinpath("fin").read_bytes(), self.new.joinpath("fin").read_bytes())
        backup = next(self.runtime.parent.glob("runtime-backup.*/previous"))
        for name in FILES:
            self.assertEqual(backup.joinpath(name).read_bytes(), self.old.joinpath(name).read_bytes())


if __name__ == "__main__":
    unittest.main(verbosity=2)
