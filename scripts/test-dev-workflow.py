#!/usr/bin/env python3
"""Make routing and no-TTY guards; never installs, elevates or loads credentials."""
import json
import os
import re
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

REPO = Path(__file__).resolve().parent.parent
MAKE = shutil.which('make')


class DevWorkflowTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='fin-make-test-')
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        shutil.copyfile(REPO / 'Makefile', self.root / 'Makefile')
        (self.root / 'scripts').mkdir()
        (self.root / 'scripts/dev-macos.sh').write_text('#!/bin/sh\nprintf "action:%s\\n" "$1"\n')
        (self.root / 'bin').mkdir()
        cargo = self.root / 'bin/cargo'
        cargo.write_text('#!/bin/sh\nprintf "arg:%s\\n" "$@"\n')
        cargo.chmod(0o700)

    def make(self, *args):
        env = dict(os.environ, PATH=str(self.root / 'bin') + ':/usr/bin:/bin')
        return subprocess.run([MAKE, *args], cwd=self.root, env=env, capture_output=True, text=True, timeout=15)

    def test_default_is_help_not_install_or_build(self):
        result = self.make()
        self.assertEqual(result.returncode, 0)
        self.assertIn('make setup', result.stdout)
        self.assertIn('HUMAN:', result.stdout)
        self.assertNotIn('action:', result.stdout)
        self.assertNotIn('arg:', result.stdout)

    def test_human_and_bundle_targets_route_explicitly(self):
        for target in ('setup', 'update', 'uninstall', 'bundle', 'release'):
            with self.subTest(target=target):
                result = self.make(target)
                self.assertEqual(result.returncode, 0)
                self.assertEqual(result.stdout, 'action:' + target + '\n')

    def test_dev_defaults_to_help_and_build_is_incremental_debug(self):
        self.assertIn('arg:--help\n', self.make('dev').stdout)
        result = self.make('build')
        self.assertEqual(result.returncode, 0)
        self.assertIn('arg:build\n', result.stdout)
        self.assertNotIn('arg:--release', result.stdout)
        self.assertNotIn('action:', result.stdout)

    def test_dev_arguments_preserve_quoted_json_as_one_argument(self):
        result = self.make('dev', 'ARGS=preview fixture.read --input \'{"account":"synthetic main"}\' --json')
        self.assertEqual(result.returncode, 0)
        self.assertIn('arg:{"account":"synthetic main"}\n', result.stdout)
        self.assertTrue(result.stdout.endswith('arg:--json\n'))

    def test_real_human_actions_refuse_pipes_before_privilege_or_packaging(self):
        if os.uname().sysname != 'Darwin':
            self.skipTest('macOS helper')
        for target in ('setup', 'update', 'uninstall'):
            result = subprocess.run(['/bin/sh', str(REPO/'scripts/dev-macos.sh'), target],
                                    stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=10)
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(result.stdout, '')
            self.assertIn('macOS Terminal', result.stderr)
            self.assertNotIn('password', result.stderr)

    def test_setup_forwards_verbose_to_packaging_and_cli_only_when_requested(self):
        source = (REPO / 'scripts/dev-macos.sh').read_text()
        fixture = self.root / 'workflow'
        scripts = fixture / 'scripts'
        mocks = fixture / 'mocks'
        scripts.mkdir(parents=True)
        mocks.mkdir()
        (fixture / 'work').mkdir()
        (fixture / 'scripts/dev-macos.sh').write_text(
            source.replace('/usr/bin/uname', str(mocks / 'uname'))
            .replace('/usr/bin/id', str(mocks / 'id'))
            .replace('/usr/bin/sudo', str(mocks / 'sudo'))
            .replace('/Library/Fin', str(fixture / 'Library/Fin'))
            .replace('/Library/LaunchDaemons', str(fixture / 'Library/LaunchDaemons'))
            .replace('/private/etc/paths.d/fin', str(fixture / 'private/etc/paths.d/fin'))
            .replace("[ -t 0 ] && [ -t 1 ] && [ -t 2 ] || { echo 'Run this target yourself in macOS Terminal, without redirection.' >&2; exit 1; }", 'true'))
        (mocks / 'uname').write_text('#!/bin/sh\necho Darwin\n')
        (mocks / 'id').write_text('#!/bin/sh\necho 501\n')
        (mocks / 'sudo').write_text('#!/bin/sh\nprintf \"sudo:%s\\n\" \"$*\" >> \"$EVENTS\"\nexit 0\n')
        for mock in mocks.iterdir():
            mock.chmod(0o700)
        (scripts / 'package-rust-macos.sh').write_text('''#!/bin/sh
printf 'package:%s\\n' "$*" >> "$EVENTS"
bundle=$1
mkdir -p "$bundle"
cat > "$bundle/fin" <<'EOF'
#!/bin/sh
printf 'fin:%s\\n' "$*" >> "$EVENTS"
EOF
chmod 700 "$bundle/fin"
''')
        (scripts / 'package-rust-macos.sh').chmod(0o700)
        events = fixture / 'events.txt'
        for verbose in (False, True):
            events.write_text('')
            args = ['/bin/sh', str(scripts / 'dev-macos.sh'), 'setup']
            if verbose:
                args.append('--verbose')
            result = subprocess.run(args, cwd=fixture, capture_output=True, text=True,
                                    timeout=10, env=dict(os.environ, EVENTS=str(events)))
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            lines = [line for line in events.read_text().splitlines() if line != 'sudo:-K']
            self.assertEqual(len(lines), 2, lines)
            self.assertTrue(lines[0].startswith('package:'), lines)
            if verbose:
                self.assertEqual(lines[1], 'fin:setup --verbose')
                self.assertIn('--dev --verbose', lines[0])
            else:
                self.assertEqual(lines[1], 'fin:setup')
                self.assertNotIn('--verbose', lines[0])

    def test_update_builds_then_invokes_reviewed_updater_and_clears_elevation(self):
        fixture = self.root / 'update-workflow'
        scripts = fixture / 'scripts'
        mocks = fixture / 'mocks'
        scripts.mkdir(parents=True)
        mocks.mkdir()
        events = fixture / 'events.txt'
        source = (REPO / 'scripts/dev-macos.sh').read_text()
        source = source.replace('/usr/bin/uname', str(mocks / 'uname')).replace('/usr/bin/id', str(mocks / 'id')).replace('/usr/bin/sudo', str(mocks / 'sudo'))
        source = source.replace('[ -t 0 ] && [ -t 1 ] && [ -t 2 ]', 'true')
        source = source.replace('/Library/Fin', str(fixture / 'Library/Fin'))
        (scripts / 'dev-macos.sh').write_text(source)
        (mocks / 'uname').write_text('#!/bin/sh\necho Darwin\n')
        (mocks / 'id').write_text('#!/bin/sh\necho 501\n')
        (mocks / 'sudo').write_text('#!/bin/sh\nprintf "sudo:%s\\n" "$*" >> "$EVENTS"\n')
        for mock in mocks.iterdir():
            mock.chmod(0o700)
        (scripts / 'package-rust-macos.sh').write_text('#!/bin/sh\nprintf "package:%s\\n" "$*" >> "$EVENTS"\n')
        result = subprocess.run(['/bin/sh', str(scripts / 'dev-macos.sh'), 'update'], cwd=fixture,
                                env=dict(os.environ, EVENTS=str(events)), capture_output=True, text=True, timeout=10)
        self.assertEqual(result.returncode, 0, result.stderr)
        lines = events.read_text().splitlines()
        self.assertEqual(len(lines), 3)
        self.assertTrue(lines[0].startswith('package:'))
        self.assertIn('--dev', lines[0])
        self.assertIn('update-rust-macos-runtime.sh', lines[1])
        self.assertIn('--from ' + str(fixture / 'Library/Fin/runtime'), lines[1])
        self.assertIn('--presentation-update', lines[1])
        self.assertEqual(lines[2], 'sudo:-K')
        self.assertNotIn('fin setup', '\n'.join(lines))

    def test_invalid_helper_action_is_rejected(self):
        result = subprocess.run(['/bin/sh', str(REPO/'scripts/dev-macos.sh'), 'anything'], capture_output=True, text=True)
        self.assertEqual(result.returncode, 2)

    def test_package_success_rows_share_full_line_color_and_plain_fallbacks(self):
        source = (REPO / 'scripts/package-rust-macos.sh').read_text()
        helper = source.split('fin_success() {', 1)[1].split("\nfin_active_pid=''", 1)[0]
        script = 'fin_verbose=0\nfin_success() {' + helper + '\nfin_run_stage "Compiling fixture" /usr/bin/true\nfin_success "Reusing unchanged native approval helper"\n'
        plain = '✓ Reusing unchanged native approval helper'
        for terminal, no_color, dumb in [(True, False, False), (True, True, False), (True, False, True), (False, False, False)]:
            with self.subTest(terminal=terminal, no_color=no_color, dumb=dumb):
                env = {'PATH': '/usr/bin:/bin', 'TERM': 'dumb' if dumb else 'xterm-256color'}
                if no_color:
                    env['NO_COLOR'] = ''
                if terminal:
                    master, slave = os.openpty()
                    try:
                        result = subprocess.run(['/bin/sh', '-c', script], stderr=slave, stdout=subprocess.PIPE, env=env, timeout=5)
                        text = os.read(master, 4096).decode().replace('\r\n', '\n')
                    finally:
                        os.close(slave)
                        os.close(master)
                else:
                    result = subprocess.run(['/bin/sh', '-c', script], capture_output=True, env=env, timeout=5)
                    text = result.stderr.decode()
                self.assertEqual(result.returncode, 0)
                expected = '\x1b[32m' + plain + '\x1b[0m\n' if terminal and not no_color and not dumb else plain + '\n'
                self.assertTrue(text.endswith(expected), text)
                self.assertNotIn('… ', text)
                # Each spinner rewrites its row with CR + erase-line. Assert the
                # final visible rows, not just that green text appeared somewhere.
                rows = [re.sub(r'\x1b\[[0-9;]*[mK]', '', row.rsplit('\r', 1)[-1]) for row in text.split('\n') if row]
                self.assertEqual(rows, ['✓ Compiling fixture', plain])


if __name__ == '__main__':
    unittest.main()
