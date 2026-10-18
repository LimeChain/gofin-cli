#!/usr/bin/env python3
"""Synthetic installer regression: temporary paths and OS/privilege doubles only.

Run with python3 scripts/test-rust-macos-installer.py. This does not establish
actual root ownership, human enrollment, launchd operation, or Touch ID behavior.
"""

import hashlib
import json
import os
from pathlib import Path
import re
import selectors
import time
import stat
import subprocess
import tempfile
import unittest


INSTALLER = Path(__file__).resolve().parents[1] / "packaging/install.sh"
DRIVER = r'''#!/usr/bin/python3
import json, os, pathlib, stat, sys
root = pathlib.Path(ROOT)
name = pathlib.Path(sys.argv[0]).name
args = sys.argv[1:]
options = json.loads((root / "options.json").read_text())
with (root / "events.jsonl").open("a") as log:
    log.write(json.dumps([name, args]) + "\n")
if name in ("mkdir", "cp", "rm", "mv", "mktemp", "chmod", "sleep"):
    # No fixture command may mutate a system path, even if rewriting breaks.
    for argument in args:
        if argument.startswith("/") and not argument.startswith(str(root) + "/"):
            sys.exit("Refusing non-fixture path: " + argument)
    os.execv("/bin/" + name if name != "mktemp" else "/usr/bin/mktemp",
             [name] + args)
elif name == "stty":
    os.execv("/bin/stty", [name] + args)
elif name in ("awk", "shasum", "cat", "cmp"):
    os.execv(("/bin/" if name == "cat" else "/usr/bin/") + name, [name] + args)
elif name == "uname":
    print("Darwin")
elif name == "id":
    print({"-u": "0", "-g": "20", "-nu": "synthetic-human"}[args[0]])
elif name == "stat":
    print(options.get("owners", {}).get(args[-1], 0))
elif name == "ls":
    path = pathlib.Path(args[-1])
    mode = stat.filemode(path.stat().st_mode)
    print(mode + ("+" if str(path) in options.get("acls", []) else ""),
          "1 root wheel 0 Jan 1 00:00", path)
elif name == "codesign":
    if "-d" in args:
        print("CodeDirectory flags=0x10000(runtime)")
elif name == "otool":
    print(args[-1] + ":\n\t/usr/lib/libSystem.B.dylib (compatibility version 1.0.0)")
elif name == "gofin-broker":
    if options.get("require_terminal"):
        if not all(os.isatty(fd) for fd in (0, 1, 2)):
            sys.exit("SYNTHETIC: enrollment descriptors must remain terminals")
        print("SYNTHETIC: enrollment inherited three terminal descriptors")
    if options.get("enrollment") == "fail-before":
        sys.exit(1)
    if options.get("enrollment") == "cancel":
        import signal
        os.kill(os.getppid(), signal.SIGTERM)
        sys.exit(1)
    (root / "Library/GoFin/private/config.json").write_text("{}\n")
    if options.get("enrollment") == "fail-after":
        sys.exit(1)
elif name == "launchctl":
    if options.get("bootstrap_failure"):
        sys.exit(1)
    if options.get("late_path_conflict"):
        (root / "private/etc/paths.d/gofin").symlink_to(root / "absent")
elif name == "sudo":
    if "internal-analytics-lifecycle" in args:
        sys.exit(1 if options.get("analytics_failure") else 0)
    if args[-1:] == ["internal-daemon-start"]:
        if not (root / "Library/GoFin/private/config.json").is_file():
            sys.exit("SYNTHETIC: broker start requires initialized configuration")
        if options.get("daemon_start_failure"):
            sys.exit("SYNTHETIC: broker failed to respond")
        print("GoFin daemon running.")
        sys.exit(0)
    marker = root / "Library/GoFin/private/completion-owner"
    if not marker.is_file() or stat.S_IMODE(marker.stat().st_mode) != 0o600 or marker.read_text() != "501\n":
        sys.exit("SYNTHETIC: completion owner marker must be secured before helper invocation")
    if options.get("completion_failure"):
        sys.exit(1)
elif name not in ("chown", "plutil"):
    sys.exit("Unexpected mock command: " + name)
'''


class InstallerFixture(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="gofin-installer-test-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name).resolve()
        self.bundle = self.root / "bundle"
        self.bin = self.root / "bin"
        self.path_file = self.root / "private/etc/paths.d/gofin"
        self.installed = self.root / "Library/GoFin"
        self.options = {}
        for path in (self.bundle, self.bin, self.path_file.parent,
                     self.root / "Library/LaunchDaemons", self.root / "usr/local/bin"):
            path.mkdir(parents=True, exist_ok=True)
        for path in (self.root / "private", self.root / "private/etc", self.path_file.parent):
            path.chmod(0o755)
        # Homebrew-owned directories must not suppress PATH registration.
        self.options["owners"] = {str(self.root / "usr/local/bin"): 501}
        driver = DRIVER.replace("ROOT", repr(str(self.root)))
        for name in ("uname", "id", "stat", "ls", "codesign", "otool", "launchctl",
                     "plutil", "chown", "mkdir", "cp", "rm", "mv", "mktemp", "chmod",
                     "awk", "shasum", "cat", "cmp", "sleep", "sudo", "stty"):
            executable = self.bin / name
            executable.write_text(driver)
            executable.chmod(0o755)
        source = INSTALLER.read_text()
        terminal_guard = '[ -t 0 ] && [ -t 1 ] && [ -t 2 ]'
        self.assertEqual(source.count(terminal_guard), 2)
        # The synthetic harness substitutes privilege/TTY/OS checks; the shipped
        # installer has no testing flag or environment-controlled bypass.
        source = source.replace(terminal_guard + " || {", "true || {")
        source = re.sub(r'(?<![\w/])/(?:Library|private|usr/local)(?=[/\s";:]|$)',
                        lambda match: str(self.root) + match.group(0), source)
        source = re.sub(r'(?<![\w/])/(?:usr/)?s?bin/([a-z]+)',
                        lambda match: str(self.bin / match.group(1)), source)
        for name in ("gofin", "gofin-touch-id", "README.md", "BUILDINFO"):
            (self.bundle / name).write_text("synthetic fixture\n")
        (self.bundle / "gofin-broker").write_text(driver)
        (self.bundle / "install.sh").write_text(source)
        names = ("gofin", "gofin-broker", "gofin-touch-id", "install.sh", "README.md", "BUILDINFO")
        (self.bundle / "SHA256SUMS").write_text("".join(
            hashlib.sha256((self.bundle / name).read_bytes()).hexdigest() + "  " + name + "\n"
            for name in names))

    def run_installer(self, verbose=False, environment=None):
        (self.root / "options.json").write_text(json.dumps(self.options))
        arguments = ["/bin/bash", str(self.bundle / "install.sh"), "--approver-uid", "501",
                     "--approver-gid", "20"]
        if verbose:
            arguments.append("--verbose")
        env = {"PATH": "/usr/bin:/bin:/usr/sbin:/sbin"}
        if environment:
            env.update(environment)
        return subprocess.run(
            # Intel CI runs many Python OS doubles more slowly; match the
            # existing PTY fixture bound without changing product deadlines.
            arguments, capture_output=True, text=True, timeout=30,
            env=env)

    def events(self):
        return [json.loads(line) for line in (self.root / "events.jsonl").read_text().splitlines()]

    def release_bundle(self):
        (self.bundle / "BUILDINFO").write_text(
            "source_state=clean\nprofile=release\nrelease_version=v0.2.0-beta.2\n")
        names = ("gofin", "gofin-broker", "gofin-touch-id", "install.sh", "README.md", "BUILDINFO")
        (self.bundle / "SHA256SUMS").write_text("".join(
            hashlib.sha256((self.bundle / name).read_bytes()).hexdigest() + "  " + name + "\n"
            for name in names))

    def analytics_events(self):
        return [args for name, args in self.events()
                if name == "sudo" and "internal-analytics-lifecycle" in args]

    def test_release_install_reports_once_before_optional_work_even_if_helpers_fail(self):
        self.release_bundle()
        self.options.update(analytics_failure=True, completion_failure=True)
        result = self.run_installer()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        calls = self.analytics_events()
        self.assertEqual([args[args.index("--outcome") + 1] for args in calls],
                         ["attempted", "succeeded"])
        self.assertEqual(calls[1][calls[1].index("--stage") + 1], "complete")
        for args in calls:
            self.assertEqual(args[:6], ["-n", "-H", "-u", "#501", str(self.bin / "env"), "-i"])
            self.assertEqual(args[6], "PATH=/usr/bin:/bin:/usr/sbin:/sbin")
            self.assertEqual(args[args.index("--kind") + 1], "install")
            self.assertEqual(args[args.index("--to-version") + 1], "v0.2.0-beta.2")
        events = self.events()
        completion = next(event for event in events if event[0] == "sudo"
                          and event[1][-2:] == ["internal-completions", "install"])
        path_registration = ["chown", ["root:wheel", str(self.path_file)]]
        self.assertLess(events.index(path_registration), events.index(["sudo", calls[1]]))
        self.assertLess(events.index(["sudo", calls[1]]), events.index(completion))

    def test_release_install_failure_reports_fixed_stage_before_removing_runtime(self):
        self.release_bundle()
        self.options["enrollment"] = "fail-before"
        result = self.run_installer()
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        calls = self.analytics_events()
        self.assertEqual([args[args.index("--outcome") + 1] for args in calls],
                         ["attempted", "failed"])
        self.assertEqual(calls[1][calls[1].index("--stage") + 1], "configure")
        self.assertFalse(self.installed.exists())

    def test_development_install_does_not_invoke_analytics_helper(self):
        result = self.run_installer()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(self.analytics_events(), [])

    def test_release_install_reports_cancellation_once(self):
        self.release_bundle()
        self.options["enrollment"] = "cancel"
        result = self.run_installer()
        self.assertEqual(result.returncode, 143, result.stdout + result.stderr)
        calls = self.analytics_events()
        self.assertEqual([args[args.index("--outcome") + 1] for args in calls],
                         ["attempted", "canceled"])
        self.assertFalse(self.installed.exists())

    def assert_preflight_refused(self):
        result = self.run_installer()
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertFalse(self.installed.exists())
        self.assertNotIn("mktemp", [event[0] for event in self.events()])
        self.assertNotIn("GoFin installed", result.stdout)
        self.assertNotIn("Optional:", result.stdout)

    def test_registers_path_after_configuration_and_bootstrap(self):
        result = self.run_installer()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(self.path_file.read_text(), str(self.installed / "runtime") + "\n")
        self.assertEqual(stat.S_IMODE(self.path_file.stat().st_mode), 0o644)
        self.assertFalse((self.root / "usr/local/bin/gofin").exists())
        helper = self.installed / "runtime/authorization/GoFin"
        yolo_gate = self.installed / "private/yolo-gate.lock"
        self.assertTrue(yolo_gate.is_file())
        self.assertEqual(stat.S_IMODE(yolo_gate.stat().st_mode), 0o600)
        self.assertEqual(helper.read_bytes(), (self.installed / "runtime/gofin-touch-id").read_bytes())
        self.assertEqual(stat.S_IMODE(helper.stat().st_mode), 0o755)
        self.assertEqual(stat.S_IMODE(helper.parent.stat().st_mode), 0o755)
        events = self.events()
        owner = ["chown", ["root:wheel", str(self.path_file)]]
        self.assertIn(owner, events)
        names = [event[0] for event in events]
        self.assertLess(names.index("gofin-broker"), names.index("launchctl"))
        self.assertLess(names.index("launchctl"), events.index(owner))
        self.assertTrue(any(name == "codesign" and "authorization/GoFin" in args[-1]
                            for name, args in events))
        self.assertTrue(any(name == "cmp" and args[-2].endswith("runtime/gofin-touch-id")
                            and args[-1].endswith("runtime/authorization/GoFin")
                            for name, args in events))
        self.assertEqual(result.stdout, "GoFin installed and running. Next: open a new Terminal and run gofin connections add.\n")
        broker_args = next(args for name, args in events if name == "gofin-broker")
        self.assertNotIn("--verbose", broker_args)
        start_args = next(args for name, args in events if name == "sudo" and args[-1:] == ["internal-daemon-start"])
        self.assertEqual(start_args, ["-n", "-H", "-u", "#501", str(self.bin / "env"),
                                    "TERM=dumb", str(self.installed / "runtime/gofin"), "internal-daemon-start"])
        start_index = events.index(["sudo", start_args])
        self.assertLess(names.index("launchctl"), start_index)
        self.assertLess(start_index, events.index(owner))
        sudo_args = next(args for name, args in events if name == "sudo" and args[-2:] == ["internal-completions", "install"])
        self.assertEqual(sudo_args[:6], ["-n", "-H", "-u", "#501", str(self.bin / "env"), "TERM=dumb"])
        self.assertEqual(sudo_args[6:], [str(self.installed / "runtime/gofin"), "internal-completions", "install"])
        marker = self.installed / "private/completion-owner"
        self.assertEqual(marker.read_text(), "501\n")
        self.assertEqual(stat.S_IMODE(marker.stat().st_mode), 0o600)
        events = self.events()
        marker_owner = ["chown", ["root:wheel", str(marker)]]
        self.assertIn(marker_owner, events)
        self.assertLess(events.index(marker_owner), events.index(["sudo", sudo_args]))
        self.assertNotIn("Arrows move; Space selects;", result.stderr)
        self.assertIn("✓ Preparing protected runtime files", result.stderr)
        self.assertIn("✓ Verifying signed binaries and system libraries", result.stderr)
        self.assertNotIn("\033[", result.stderr)

    def test_verbose_adds_setup_details_and_passes_flag_to_enrollment(self):
        result = self.run_installer(verbose=True, environment={"TERM": "xterm-256color", "NO_COLOR": "1"})
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(result.stdout, "GoFin installed and running. Next: open a new Terminal and run gofin connections add.\n")
        self.assertIn("GoFin setup: bundle", result.stderr)
        self.assertIn("GoFin setup: protected configuration and operation journal", result.stderr)
        self.assertNotIn("credential verification", result.stderr.lower())
        self.assertNotIn("\033[", result.stderr)
        broker_args = next(args for name, args in self.events() if name == "gofin-broker")
        self.assertEqual(broker_args[-1], "--verbose")
        sudo_args = next(args for name, args in self.events() if name == "sudo" and args[-2:] == ["internal-completions", "install"])
        self.assertEqual(sudo_args[5:8], ["TERM=xterm-256color", "NO_COLOR=1", str(self.installed / "runtime/gofin")])

    def test_existing_fin_private_state_is_preserved_before_any_installation(self):
        legacy_root = self.root / "Library/Fin"
        credentials = legacy_root / "private/credentials"
        credentials.mkdir(parents=True)
        secret = credentials / "synthetic"
        secret.write_bytes(b"synthetic legacy credential")
        result = self.run_installer()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("reviewed upgrade", result.stderr)
        self.assertEqual(secret.read_bytes(), b"synthetic legacy credential")
        self.assertFalse(self.installed.exists())
        self.assertFalse(any(name in ("mktemp", "mv", "gofin-broker", "launchctl") for name, _ in self.events()))

    def test_completion_install_failure_warns_without_undoing_installation(self):
        self.options["completion_failure"] = True
        result = self.run_installer()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertTrue(self.installed.exists())
        self.assertTrue(self.path_file.exists())
        self.assertIn("shell completion setup did not finish", result.stderr)

    def test_daemon_start_failure_preserves_configuration_without_claiming_success(self):
        self.options["daemon_start_failure"] = True
        result = self.run_installer()
        self.assertNotEqual(result.returncode, 0)
        self.assertTrue((self.installed / "private/config.json").is_file())
        self.assertFalse(self.path_file.exists())
        self.assertNotIn("GoFin installed", result.stdout)
        self.assertIn("broker failed to respond", result.stderr)

    def check_enrollment_terminal_descriptors(self, verbose, no_color=False):
        self.options["require_terminal"] = True
        (self.root / "options.json").write_text(json.dumps(self.options))
        arguments = ["/bin/bash", str(self.bundle / "install.sh"), "--approver-uid", "501",
                     "--approver-gid", "20"]
        if verbose:
            arguments.append("--verbose")
        master, slave = os.openpty()
        try:
            env = {"PATH": "/usr/bin:/bin:/usr/sbin:/sbin", "TERM": "xterm-256color"}
            if no_color:
                env["NO_COLOR"] = ""
            process = subprocess.Popen(arguments, stdin=slave, stdout=slave, stderr=slave, env=env)
            # Drain while running: verbose output can fill macOS's small PTY
            # buffer. Keep our slave open until the final output is collected.
            os.set_blocking(master, False)
            chunks = []
            answered = False
            deadline = time.monotonic() + 30
            with selectors.DefaultSelector() as ready:
                ready.register(master, selectors.EVENT_READ)
                while True:
                    ready.select(timeout=0.1)
                    while True:
                        try:
                            data = os.read(master, 4096)
                        except BlockingIOError:
                            break
                        if not data:
                            break
                        chunks.append(data)
                        if not answered and b"Arrows move; Space selects;" in b"".join(chunks):
                            os.write(master, b"\n")
                            answered = True
                    if process.poll() is not None:
                        break
                    if time.monotonic() >= deadline:
                        process.kill()
                        process.wait()
                        partial = b"".join(chunks).decode(errors="replace")
                        self.fail(f"Synthetic installer timed out; captured output:\n{partial}")
            output = b"".join(chunks).decode()
            self.assertEqual(process.returncode, 0, output)
            self.assertIn("enrollment inherited three terminal descriptors", output)
            self.assertIn("GoFin installed and running.", output)
            self.assertTrue(answered)
            self.assertEqual(output.count("Install GoFin's optional skill"), 1)
            self.assertLess(output.index("GoFin installed and running."), output.index("Arrows move; Space selects;"))
            self.assertFalse(any("skill" in args for name, args in self.events() if name == "sudo"))
            self.assertIn("Registering GoFin for new Terminal sessions", output)
            self.assertNotIn("… ", output)
            self.assertEqual(output.count("Installing protected runtime"), 1)
            rows = [re.sub(r'\x1b\[[0-9;]*[mK]', '', row.rsplit('\r', 1)[-1])
                    for row in output.replace('\r\n', '\n').split('\n') if row]
            self.assertEqual(rows.count("✓ Installing protected runtime"), 1)
            self.assertFalse(any(row.startswith(("… ", "⠋", "⠙", "⠹", "⠸")) for row in rows))
            self.assertNotIn("credential verification", output.lower())
            if no_color:
                self.assertNotIn("\x1b[", output)
            else:
                self.assertIn("\x1b[36m⠋ Preparing protected runtime files\x1b[0m", output)
                self.assertIn("\x1b[32m✓ Preparing protected runtime files\x1b[0m", output)
        finally:
            os.close(slave)
            os.close(master)

    def test_default_setup_preserves_enrollment_terminal_descriptors(self):
        self.check_enrollment_terminal_descriptors(False)

    def test_verbose_setup_preserves_enrollment_terminal_descriptors(self):
        self.check_enrollment_terminal_descriptors(True)

    def test_no_color_is_respected_in_human_terminal(self):
        self.check_enrollment_terminal_descriptors(False, no_color=True)

    def test_existing_path_file_is_preserved_before_staging(self):
        self.path_file.write_text("preserve\n")
        self.assert_preflight_refused()
        self.assertEqual(self.path_file.read_text(), "preserve\n")

    def test_dangling_path_symlink_is_preserved_before_staging(self):
        self.path_file.symlink_to(self.root / "absent")
        self.assert_preflight_refused()
        self.assertTrue(self.path_file.is_symlink())

    def test_existing_legacy_cli_conflict_is_refused(self):
        (self.root / "usr/local/bin/gofin").symlink_to(self.root / "absent")
        self.assert_preflight_refused()

    def test_each_system_directory_requires_root_ownership(self):
        for relative in ("private", "private/etc", "private/etc/paths.d"):
            with self.subTest(directory=relative):
                self.options["owners"][str(self.root / relative)] = 501
                self.assert_preflight_refused()
                del self.options["owners"][str(self.root / relative)]

    def test_group_and_world_write_are_refused(self):
        for mode in (0o775, 0o757):
            with self.subTest(mode=oct(mode)):
                self.path_file.parent.chmod(mode)
                self.assert_preflight_refused()

    def test_extended_acl_is_refused(self):
        self.options["acls"] = [str(self.path_file.parent)]
        self.assert_preflight_refused()

    def test_symbolic_link_parent_is_refused(self):
        self.path_file.parent.rmdir()
        alternative = self.root / "alternative"
        alternative.mkdir()
        self.path_file.parent.symlink_to(alternative, target_is_directory=True)
        self.assert_preflight_refused()

    def test_missing_paths_directory_is_refused_without_creation(self):
        self.path_file.parent.rmdir()
        self.assert_preflight_refused()
        self.assertFalse(self.path_file.parent.exists())

    def test_configuration_failures_never_register_or_claim_success(self):
        for failure in ("fail-before", "fail-after"):
            with self.subTest(failure=failure):
                self.options["enrollment"] = failure
                result = self.run_installer()
                self.assertNotEqual(result.returncode, 0)
                self.assertFalse(self.path_file.exists())
                self.assertNotIn("GoFin installed", result.stdout)
                self.assertEqual(self.installed.exists(), failure == "fail-after")
                if failure == "fail-after":
                    self.assertIn("Configuration may already be committed", result.stderr)

    def test_bootstrap_failure_preserves_configuration_without_path_file(self):
        self.options["bootstrap_failure"] = True
        result = self.run_installer()
        self.assertNotEqual(result.returncode, 0)
        self.assertTrue((self.installed / "private/config.json").exists())
        self.assertFalse(self.path_file.exists())
        self.assertNotIn("GoFin installed", result.stdout)

    def test_late_conflict_preserves_configuration_and_symlink(self):
        self.options["late_path_conflict"] = True
        result = self.run_installer()
        self.assertNotEqual(result.returncode, 0)
        self.assertTrue((self.installed / "private/config.json").exists())
        self.assertTrue(self.path_file.is_symlink())
        self.assertNotIn("GoFin installed", result.stdout)
        self.assertIn("Protected installation was preserved", result.stderr)


class SkillOfferFixture(unittest.TestCase):
    """Exercise the real optional prompt in disposable PTYs, never real sudo."""
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="gofin-skill-offer-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.events = self.root / "events.txt"
        mock = self.root / "sudo"
        mock.write_text('#!/bin/bash\nprintf "%s\\n" "$@" >> "$EVENTS"\n'
                        'if [ "${SKILL_FAIL_AGENT:-}" = "${!#}" ]; then exit 1; fi\n'
                        'exit "${SKILL_RESULT:-0}"\n')
        mock.chmod(0o700)
        source = "restore_companion_terminal() {" + INSTALLER.read_text().split("restore_companion_terminal() {", 1)[1].split("\ngofin_active_pid=''", 1)[0]
        self.script = self.root / "offer.sh"
        self.script.write_text("set -eu\ngofin_uid=501\ntrap 'exit 130' INT\ntrap restore_companion_terminal EXIT\n" +
                               source.replace('/usr/bin/sudo', str(mock)) +
                               "\necho 'SYNTHETIC setup complete'\noffer_companion_skill\n")

    def offer(self, answer, no_color=False, result=0, redirected=False, dumb=False,
              fail_agent=None):
        import signal
        env = {"PATH":"/usr/bin:/bin", "HOME":str(self.root), "TERM":"dumb" if dumb else "xterm-256color",
               "EVENTS":str(self.events), "SKILL_RESULT":str(result)}
        if no_color:
            env["NO_COLOR"] = "1"
        if fail_agent:
            env["SKILL_FAIL_AGENT"] = fail_agent
        if redirected:
            run = subprocess.run(['/bin/bash', str(self.script)], input='yes\n', capture_output=True, text=True, env=env, timeout=5)
            return run.returncode, run.stdout + run.stderr
        master, slave = os.openpty()
        import termios
        initial_terminal = termios.tcgetattr(slave)
        process = subprocess.Popen(['/bin/bash', str(self.script)], stdin=slave, stdout=slave, stderr=slave, env=env)
        try:
            os.set_blocking(master, False)
            chunks = []
            replied = False
            deadline = time.monotonic() + 5
            with selectors.DefaultSelector() as ready:
                ready.register(master, selectors.EVENT_READ)
                while True:
                    ready.select(timeout=0.05)
                    while True:
                        try:
                            data = os.read(master, 4096)
                        except BlockingIOError:
                            break
                        if not data:
                            break
                        chunks.append(data)
                    if (not replied and b"1/4 > [ ] Codex" in b"".join(chunks)
                            and not (termios.tcgetattr(slave)[3] & termios.ICANON)):
                        if answer == 'cancel':
                            process.send_signal(signal.SIGINT)
                        else:
                            os.write(master, answer)
                        replied = True
                    if process.poll() is not None:
                        break
                    if time.monotonic() > deadline:
                        self.fail("Optional prompt timed out")
            self.assertTrue(replied)
            output = b"".join(chunks).decode()
            self.assertEqual(output.count("Arrows move; Space selects;"), 1)
            self.assertNotIn("\x1b[", output)
            restored_terminal = termios.tcgetattr(slave)
            # Darwin sets PENDIN when canonical mode is restored; it is kernel
            # input reprocessing state, not a changed user terminal setting.
            restored_terminal[3] &= ~getattr(termios, "PENDIN", 0)
            initial_terminal[3] &= ~getattr(termios, "PENDIN", 0)
            self.assertEqual(restored_terminal, initial_terminal, "Terminal settings were not restored")
            return process.returncode, output
        finally:
            if process.poll() is None:
                process.kill()
                process.wait()
            os.close(slave)
            os.close(master)

    def test_choices_install_as_human_without_backup_or_additional_approval(self):
        for answer, agents in ((b' \n', ['codex']),
                               (b'\x1b[B \n', ['claude']),
                               (b'\x1b[B\x1b[B \n', ['hermes']),
                               (b'\x1b[B\x1b[B\x1b[B \n', ['openclaw']),
                               (b' \x1b[B \n', ['codex', 'claude']),
                               (b' \x1bOB \x1bOA \n', ['claude']),
                               (b'\x1b[C \x1b[D \n', ['codex', 'claude']),
                               (b' \x1b[B \x1b[B \x1b[B \n',
                                ['codex', 'claude', 'hermes', 'openclaw'])):
            with self.subTest(answer=answer):
                self.events.unlink(missing_ok=True)
                status, output = self.offer(answer, no_color=True)
                self.assertEqual(status, 0, output)
                expected = []
                for agent in agents:
                    expected.extend([
                        '-n', '-H', '-u', '#501', '/usr/bin/env', 'TERM=xterm-256color', 'NO_COLOR=1',
                        '/Library/GoFin/runtime/gofin', 'skill', 'install', '--agent', agent])
                self.assertEqual(self.events.read_text().splitlines(), expected)

    def test_directional_navigation_wraps_in_both_escape_formats(self):
        for prefix in (b'\x1b[', b'\x1bO'):
            for backwards, forwards in ((b'A', b'B'), (b'D', b'C')):
                with self.subTest(prefix=prefix, backwards=backwards):
                    self.events.unlink(missing_ok=True)
                    status, output = self.offer(prefix + backwards + b' ' +
                                                prefix + forwards + b' \n')
                    self.assertEqual(status, 0, output)
                    self.assertIn('4/4 > [x] OpenClaw', output)
                    self.assertIn('1/4 > [x] Codex', output)
                    calls = self.events.read_text().splitlines()
                    self.assertEqual([value for value in calls
                                      if value in ('codex', 'claude', 'hermes', 'openclaw')],
                                     ['codex', 'openclaw'])

    def test_selection_redraw_stays_compact(self):
        status, output = self.offer(b' \x1b[B \x1b[B \x1b[B \n', dumb=True)
        self.assertEqual(status, 0, output)
        rows = [row for row in output.splitlines() if re.match(r'[1-4]/4 > ', row)]
        self.assertTrue(rows)
        self.assertTrue(all(len(row) <= 40 for row in rows), rows)
        self.assertIn('(4 selected)', rows[-1])

    def test_default_no_invalid_and_eof_do_not_install(self):
        for answer in (b'\n', b'  \n', b'anything\n', b'\x04', b' \x04', b'\x1b', b' \x1b',
                       b'\x1b[B\x1b[B  \n', b'\x1b[A \x04', b'\x1b[A \x1b'):
            with self.subTest(answer=answer):
                status, output = self.offer(answer)
                self.assertEqual(status, 0, output)
                self.assertFalse(self.events.exists())

    def test_optional_failure_leaves_successful_setup_intact(self):
        status, output = self.offer(b' \x1b[B \x1b[B \x1b[B \n', result=1)
        self.assertEqual(status, 0, output)
        self.assertIn('SYNTHETIC setup complete', output)
        self.assertIn('GoFin is installed; optional skill installation did not finish.', output)
        self.assertIn('gofin skill inspect --agent codex', output)
        self.assertIn('gofin skill inspect --agent claude', output)
        self.assertIn('gofin skill inspect --agent hermes', output)
        self.assertIn('gofin skill inspect --agent openclaw', output)
        self.assertEqual(self.events.read_text().splitlines().count('install'), 4)

    def test_each_target_failure_does_not_skip_other_selected_targets(self):
        agents = ['codex', 'claude', 'hermes', 'openclaw']
        for failed in agents:
            with self.subTest(agent=failed):
                self.events.unlink(missing_ok=True)
                status, output = self.offer(b' \x1b[B \x1b[B \x1b[B \n', fail_agent=failed)
                self.assertEqual(status, 0, output)
                calls = self.events.read_text().splitlines()
                self.assertEqual([value for value in calls if value in agents], agents)
                self.assertEqual(output.count('optional skill installation did not finish.'), 1)
                self.assertIn('gofin skill inspect --agent ' + failed, output)

    def test_cancellation_never_installs(self):
        status, output = self.offer('cancel')
        self.assertEqual(status, 130, output)
        self.assertIn('SYNTHETIC setup complete', output)
        self.assertFalse(self.events.exists())

    def test_dumb_terminal_uses_plain_lines_and_space_selection(self):
        status, output = self.offer(b'\x1b[B\x1b[B \x1b[B \n', dumb=True)
        self.assertEqual(status, 0, output)
        self.assertIn('[x] Hermes', output)
        self.assertIn('[x] OpenClaw', output)
        self.assertIn('hermes', self.events.read_text().splitlines())
        self.assertIn('openclaw', self.events.read_text().splitlines())
        self.assertNotIn('codex', self.events.read_text().splitlines())
        self.assertNotIn('claude', self.events.read_text().splitlines())

    def test_redirected_input_never_prompts_or_installs(self):
        status, output = self.offer(None, redirected=True)
        self.assertEqual(status, 0, output)
        self.assertNotIn('Arrows move; Space selects;', output)
        self.assertFalse(self.events.exists())


if __name__ == "__main__":
    unittest.main(verbosity=2)
