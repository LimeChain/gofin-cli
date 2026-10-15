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
        self.temp = tempfile.TemporaryDirectory(prefix='gofin-make-test-')
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        shutil.copyfile(REPO / 'Makefile', self.root / 'Makefile')
        (self.root / 'scripts').mkdir()
        (self.root / 'scripts/dev-macos.sh').write_text('#!/bin/sh\nprintf "action:%s\\n" "$1"\n')
        (self.root / 'bin').mkdir()
        cargo = self.root / 'bin/cargo'
        cargo.write_text('#!/bin/sh\nprintf "arg:%s\\n" "$@"\n')
        cargo.chmod(0o700)
        (self.root / 'scripts/gofin-dev.py').write_text(
            'import json,sys\nprint(json.dumps(sys.argv[1:]))\n'
            'sys.exit(7 if "unavailable" in sys.argv else 0)\n')

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

    def test_environment_lifecycle_routes_name_and_machine_flag(self):
        for action in ('start', 'stop', 'reset'):
            result = self.make('dev-env-' + action)
            self.assertEqual(result.returncode, 0, result.stderr)
            suffix = ['--mode', 'synthetic'] if action == 'start' else []
            self.assertEqual(json.loads(result.stdout), [action, 'local', *suffix])
            result = self.make('dev-env-' + action, 'DEV_ENV=feature-a', 'DEV_FLAGS=--json')
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(json.loads(result.stdout), ['--json', action, 'feature-a', *suffix])

    def test_live_mode_and_credential_file_path_are_forwarded_without_reading_secrets(self):
        result = self.make('dev-env-start', 'DEV_ENV=live', 'DEV_MODE=live', 'DEV_FLAGS=--json')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout), ['--json', 'start', 'live', '--mode', 'live'])
        result = self.make('dev-env-import', 'DEV_ENV=live', 'DEV_FLAGS=--json',
                           'DEV_CREDENTIALS_FILE=/synthetic path/fixture.env')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout), ['--json', 'import', 'live',
                         '--credentials-file', '/synthetic path/fixture.env'])
        discovery = self.make('dev-env-import', 'DEV_ENV=live',
                              'DEV_CREDENTIALS_FILE=/synthetic path/fixture.env',
                              'DEV_IMPORT_FLAGS=--discover-identities')
        self.assertEqual(discovery.returncode, 0, discovery.stderr)
        self.assertEqual(json.loads(discovery.stdout), ['import', 'live',
                         '--credentials-file', '/synthetic path/fixture.env', '--discover-identities'])
        missing = self.make('dev-env-import')
        self.assertNotEqual(missing.returncode, 0)
        self.assertEqual(missing.stdout, '')
        self.assertIn('DEV_CREDENTIALS_FILE', missing.stderr)

    def test_environment_cli_preserves_json_arguments_and_failure(self):
        result = self.make('dev-env-cli', 'DEV_ENV=feature-a',
                           'ARGS=read synthetic.echo --input \'{"value":"two words"}\' --json')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout), ['cli', 'feature-a', '--', 'read',
                         'synthetic.echo', '--input', '{"value":"two words"}', '--json'])
        result = self.make('dev-env-cli', 'DEV_ENV=unavailable', 'ARGS=operations --json')
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(json.loads(result.stdout), ['cli', 'unavailable', '--', 'operations', '--json'])

    def test_environment_tests_and_fork_are_separate_targets(self):
        scripts = (
            'test-gofin-dev.py', 'test-dev-runtime.py', 'test-solana-rehearsal.py',
            'test-fork-runner.py', 'test-base-aave-fork-runner.py',
            'test-amm-fork-runner.py', 'test-cctp-fork-runner.py', 'test-dev-workflow.py',
        )
        for script in scripts:
            (self.root / 'scripts' / script).write_text(
                'import json,sys\n'
                f'print(json.dumps({{"script": {script!r}, "args": sys.argv[1:]}}))\n'
            )
        (self.root / 'scripts/verify-rust-fork.sh').write_text('#!/bin/sh\nprintf "fork-fixture\\n"\n')
        result = self.make('test-dev-env')
        self.assertEqual(result.returncode, 0, result.stderr)
        invocations = [json.loads(line) for line in result.stdout.splitlines()]
        expected = [
            {'script': 'test-gofin-dev.py', 'args': []},
            {'script': 'test-dev-runtime.py', 'args': []},
            {'script': 'test-solana-rehearsal.py', 'args': []},
            {'script': 'test-solana-rehearsal.py',
             'args': ['--expiry-only', 'revalidate-once', '--expiry-route', 'router']},
            {'script': 'test-solana-rehearsal.py',
             'args': ['--expiry-only', 'dispatch-once', '--expiry-route', 'router']},
            {'script': 'test-solana-rehearsal.py',
             'args': ['--expiry-only', 'revalidate-every', '--expiry-route', 'router']},
            {'script': 'test-solana-rehearsal.py',
             'args': ['--expiry-only', 'dispatch-every', '--expiry-route', 'router']},
            {'script': 'test-solana-rehearsal.py',
             'args': ['--expiry-only', 'uncertain', '--expiry-route', 'router']},
            {'script': 'test-solana-rehearsal.py',
             'args': ['--expiry-only', 'revalidate-once', '--expiry-route', 'meta_aggregator']},
            {'script': 'test-solana-rehearsal.py',
             'args': ['--expiry-only', 'dispatch-once', '--expiry-route', 'meta_aggregator']},
            {'script': 'test-solana-rehearsal.py',
             'args': ['--expiry-only', 'revalidate-every', '--expiry-route', 'meta_aggregator']},
            {'script': 'test-solana-rehearsal.py',
             'args': ['--expiry-only', 'dispatch-every', '--expiry-route', 'meta_aggregator']},
            {'script': 'test-solana-rehearsal.py',
             'args': ['--expiry-only', 'uncertain', '--expiry-route', 'meta_aggregator']},
            {'script': 'test-solana-rehearsal.py', 'args': ['--perps-only']},
            {'script': 'test-solana-rehearsal.py', 'args': ['--perps-collateral-only']},
            {'script': 'test-solana-rehearsal.py', 'args': ['--perps-limit-only']},
            {'script': 'test-solana-rehearsal.py', 'args': ['--perps-limit-plan-only']},
            {'script': 'test-solana-rehearsal.py', 'args': ['--perps-tpsl-only']},
            {'script': 'test-solana-rehearsal.py', 'args': ['--perps-tpsl-paired-only']},
            {'script': 'test-solana-rehearsal.py', 'args': ['--perps-tpsl-plan-only']},
            {'script': 'test-solana-rehearsal.py', 'args': ['--jlp-only']},
            {'script': 'test-solana-rehearsal.py', 'args': ['--trigger-only']},
            {'script': 'test-solana-rehearsal.py', 'args': ['--trigger-fill-only']},
            {'script': 'test-fork-runner.py', 'args': []},
            {'script': 'test-base-aave-fork-runner.py', 'args': []},
            {'script': 'test-amm-fork-runner.py', 'args': []},
            {'script': 'test-cctp-fork-runner.py', 'args': []},
            {'script': 'test-dev-workflow.py', 'args': []},
        ]
        self.assertEqual(invocations, expected)
        result = self.make('test-fork')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, 'fork-fixture\n')

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
cat > "$bundle/gofin" <<'EOF'
#!/bin/sh
printf 'gofin:%s\\n' "$*" >> "$EVENTS"
EOF
chmod 700 "$bundle/gofin"
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
                self.assertEqual(lines[1], 'gofin:setup --verbose')
                self.assertIn('--dev --verbose', lines[0])
            else:
                self.assertEqual(lines[1], 'gofin:setup')
                self.assertNotIn('--verbose', lines[0])

    def update_fixture(self, options):
        import sys
        fixture = self.root / 'update workflow'
        scripts, mocks = fixture / 'scripts', fixture / 'mocks'
        scripts.mkdir(parents=True)
        mocks.mkdir()
        runtime = fixture / 'Library/Fin/runtime'
        runtime.mkdir(parents=True)
        private = fixture / 'Library/Fin/private'
        private.mkdir()
        (private / 'config.json').write_bytes(b'synthetic saved connections')
        (private / 'operations.sqlite').write_bytes(b'synthetic pending financial history')
        (private / 'credentials').write_bytes(b'synthetic credentials only')
        self.private_before = {p.name: p.read_bytes() for p in private.iterdir()}
        options_file, events = fixture / 'options.json', fixture / 'events.jsonl'
        options_file.write_text(json.dumps(options))
        driver = r"""
import json,os,subprocess,sys
from pathlib import Path
name=Path(sys.argv[0]).name
args=sys.argv[1:]
if name == 'driver.py': name=args.pop(0)
options_path=Path(os.environ['OPTIONS'])
options=json.loads(options_path.read_text())
with Path(os.environ['EVENTS']).open('a') as log: log.write(json.dumps([name,args])+'\n')
if name=='uname': print('Darwin')
elif name=='id': print('501')
elif name=='sudo':
    if args==['-K']: sys.exit(0)
    if options.get('fail')=='sudo': sys.exit(130)
    assert args[:4]==['-k','-p','Mac login password for %p: ','--'], args
    sys.exit(subprocess.run(args[4:]).returncode)
elif name=='package':
    if options.get('fail')=='package': sys.exit(1)
    bundle=Path(args[0]); bundle.mkdir(parents=True)
    binary=bundle/'gofin'; binary.write_text(Path(os.environ['GOFIN_FIXTURE']).read_text()); binary.chmod(0o700)
elif name=='update':
    assert args[0]=='--from' and args[2]=='--to' and args[4]=='--presentation-update', args
    if options.get('fail')=='update': sys.exit(1)
    (Path(args[1])/'gofin').write_bytes((Path(args[3])/'gofin').read_bytes())
    (Path(args[1])/'gofin').chmod(0o700)
    print('✓ GoFin updated.')
elif name in ('gofin', 'fin'):
    assert args[:1]==['daemon'], args
    action=args[1]
    if options.get('fail')==action: sys.exit(1)
    if action=='status' and options.get('bad_status'):
        print('invalid status'); sys.exit(0)
    if action=='stop': options['state']='stopped'
    elif action=='start': options['state']='running'
    options_path.write_text(json.dumps(options))
    if '--json' in args:
        print(json.dumps({'status':options['state'],'registered':options.get('registered',True)}))
    else: print('✓ GoFin daemon '+options['state']+'.')
else: sys.exit('unexpected command: '+name)
"""
        source = (REPO / 'scripts/dev-macos.sh').read_text()
        for name in ('uname', 'id', 'sudo'):
            source = source.replace('/usr/bin/' + name, '"' + str(mocks / name) + '"')
            mock = mocks / name
            mock.write_text('#!' + sys.executable + '\n' + driver)
            mock.chmod(0o700)
        source = source.replace('/Library/Fin', str(fixture / 'Library/Fin'))
        source = source.replace("[ -t 0 ] && [ -t 1 ] && [ -t 2 ] || { echo 'Run this target yourself in macOS Terminal, without redirection.' >&2; exit 1; }", 'true')
        (scripts / 'dev-macos.sh').write_text(source)
        driver_file = fixture / 'driver.py'
        driver_file.write_text(driver)
        for script, action in [('package-rust-macos.sh', 'package'), ('update-rust-macos-runtime.sh', 'update')]:
            (scripts / script).write_text('#!/bin/sh\nexec "' + sys.executable + '" "$DRIVER" ' + action + ' "$@"\n')
        binary = runtime / ('fin' if options.get('legacy') else 'gofin')
        binary.write_text('#!' + sys.executable + '\n' + driver)
        binary.chmod(0o700)
        result = subprocess.run(['/bin/sh', str(scripts / 'dev-macos.sh'), 'update'],
                                capture_output=True, text=True, timeout=15,
                                env=dict(os.environ, OPTIONS=str(options_file), EVENTS=str(events),
                                         DRIVER=str(driver_file), GOFIN_FIXTURE=str(binary)))
        self.assertEqual(self.private_before, {p.name: p.read_bytes() for p in private.iterdir()})
        recorded = [json.loads(line) for line in events.read_text().splitlines()] if events.exists() else []
        self.assertNotIn('setup', result.stdout)
        self.assertFalse(any(str(private) in arg for _, args in recorded for arg in args))
        return result, recorded, json.loads(options_file.read_text())

    def test_update_preserves_private_state_and_restores_service_state(self):
        for state in ('running', 'stopped'):
            with self.subTest(state=state), tempfile.TemporaryDirectory() as root:
                self.root = Path(root)
                result, events, observed = self.update_fixture({'state':state})
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertEqual(observed['state'], state)
                service = [args for name,args in events if name=='gofin']
                self.assertEqual(service, [['daemon','status','--json'], ['daemon','stop'],
                                           ['daemon','start'] if state=='running' else ['daemon','status']])
                names = [name for name,_ in events]
                self.assertLess(names.index('package'), names.index('gofin'))
                self.assertLess(names.index('update'), max(i for i,name in enumerate(names) if name=='gofin'))
                clears = [i for i,(name,args) in enumerate(events) if name=='sudo' and args==['-K']]
                self.assertTrue(clears)
                self.assertLess(clears[0], max(i for i,name in enumerate(names) if name=='gofin'))
                self.assertIn('Saved connections are kept', result.stderr)
                self.assertIn('Touch ID helper', result.stderr)

    def test_update_uses_legacy_cli_before_migration_and_gofin_afterwards(self):
        result, events, observed = self.update_fixture({'state': 'running', 'legacy': True})
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        old_commands = [args for name, args in events if name == 'fin']
        new_commands = [args for name, args in events if name == 'gofin']
        self.assertEqual(old_commands, [['daemon', 'status', '--json'], ['daemon', 'stop']])
        self.assertEqual(new_commands, [['daemon', 'start']])
        self.assertEqual(observed['state'], 'running')

    def test_update_failures_never_resume_or_claim_unverified_success(self):
        for failure in ('package','status','stop','sudo','update','start'):
            with self.subTest(failure=failure), tempfile.TemporaryDirectory() as root:
                self.root = Path(root)
                result, events, observed = self.update_fixture({'state':'running','fail':failure})
                self.assertNotEqual(result.returncode, 0)
                self.assertTrue(any(name=='sudo' and args==['-K'] for name,args in events))
                if failure in ('package','status','stop'):
                    self.assertFalse(any(name=='update' for name,_ in events))
                if failure in ('sudo','update'):
                    self.assertEqual(observed['state'], 'stopped')
                    self.assertFalse(any(name=='gofin' and args==['daemon','start'] for name,args in events))
                    self.assertIn('GoFin remains stopped', result.stderr)
                if failure=='sudo': self.assertEqual(result.returncode,130)
                if failure=='start': self.assertEqual(observed['state'],'stopped')

    def test_update_rejects_invalid_or_unregistered_service_before_stopping(self):
        for options in ({'state':'running','bad_status':True}, {'state':'running','registered':False}):
            with self.subTest(options=options), tempfile.TemporaryDirectory() as root:
                self.root = Path(root)
                result, events, _ = self.update_fixture(options)
                self.assertNotEqual(result.returncode,0)
                self.assertFalse(any(name=='update' or name=='gofin' and args==['daemon','stop'] for name,args in events))
    def test_invalid_helper_action_is_rejected(self):
        result = subprocess.run(['/bin/sh', str(REPO/'scripts/dev-macos.sh'), 'anything'], capture_output=True, text=True)
        self.assertEqual(result.returncode, 2)

    def test_uninstall_password_explanation_names_data_loss_and_external_effects(self):
        source = (REPO / 'scripts/dev-macos.sh').read_text()
        uninstall = source.split('if [ "$gofin_action" = uninstall ]; then', 1)[1].split('\nfi', 1)[0]
        self.assertIn('Mac login password', uninstall)
        self.assertIn('financial history', uninstall)
        self.assertIn('pending recovery records', uninstall)
        self.assertIn('does not cancel existing transactions or orders on exchanges or chains', uninstall)

    def test_package_success_rows_share_full_line_color_and_plain_fallbacks(self):
        source = (REPO / 'scripts/package-rust-macos.sh').read_text()
        helper = source.split('gofin_success() {', 1)[1].split("\ngofin_active_pid=''", 1)[0]
        script = 'gofin_verbose=0\ngofin_success() {' + helper + '\ngofin_run_stage "Compiling fixture" /usr/bin/true\ngofin_success "Reusing unchanged native approval helper"\n'
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
