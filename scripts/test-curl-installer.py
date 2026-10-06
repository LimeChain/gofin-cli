#!/usr/bin/env python3
"""Synthetic public-download fixtures; never network, install, sudo or use secrets."""
import fcntl
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import pty
import select
import shutil
import signal
import subprocess
import sys
import tarfile
import tempfile
import termios
import time
import unittest

REPO = Path(__file__).resolve().parent.parent
FILES = ('fin', 'fin-broker', 'fin-touch-id', 'install.sh', 'README.md', 'BUILDINFO')
LINUX_FILES = ('fin', 'README.md', 'BUILDINFO')
VERSION = 'v0.2.0-beta.1'
TARGET = 'aarch64-apple-darwin'
REPOSITORY = 'https://github.com/LimeChain/fin-cli'


def checksum(data):
    return hashlib.sha256(data).hexdigest()


class InstallerTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix='fin-curl-test-')
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.server = self.root / 'public-assets'
        self.bin = self.root / 'bin'
        self.server.mkdir()
        self.bin.mkdir()
        self.events = self.root / 'events'
        self.calls = self.root / 'calls'
        self.env = dict(os.environ, PATH=str(self.bin) + os.pathsep + os.environ['PATH'],
                        MOCK_SERVER=str(self.server), MOCK_CALLS=str(self.calls),
                        MOCK_EVENTS=str(self.events), MOCK_SYSTEM='Darwin', MOCK_MACHINE='arm64',
                        MOCK_UID='501', MOCK_LATEST=REPOSITORY + '/releases/tag/' + VERSION,
                        TERM='xterm-256color')
        self.env.pop('NO_COLOR', None)
        self.script = self.root / 'install.sh'
        source = (REPO / 'scripts/install.sh').read_text()
        source = source.replace('/Library/Fin', str(self.root / 'Library/Fin'))
        source = source.replace('/Library/LaunchDaemons', str(self.root / 'Library/LaunchDaemons'))
        source = source.replace('/private/etc/paths.d/fin', str(self.root / 'paths/fin'))
        source = source.replace('/usr/bin/sudo', str(self.bin / 'sudo'))
        self.script.write_text(source)
        self.mock('id', '#!/bin/sh\nprintf "%s\\n" "$MOCK_UID"\n')
        self.mock('uname', '#!/bin/sh\ncase "$1" in -s) echo "$MOCK_SYSTEM";; -m) echo "$MOCK_MACHINE";; esac\n')
        self.mock('sudo', '#!/bin/sh\nprintf "sudo:%s\\n" "$*" >> "$MOCK_EVENTS"\n')
        self.mock('curl', '#!' + sys.executable + '''
import json, os, shutil, sys, time
from pathlib import Path
args=sys.argv[1:]
with open(os.environ['MOCK_CALLS'],'a') as log: log.write(json.dumps(args)+'\\n')
if os.environ.get('MOCK_CURL_PID'): Path(os.environ['MOCK_CURL_PID']).write_text(str(os.getpid()))
if os.environ.get('MOCK_CURL_DELAY'): time.sleep(float(os.environ['MOCK_CURL_DELAY']))
if os.environ.get('MOCK_CURL_FAIL'): sys.exit(22)
if '--head' in args:
    sys.stdout.write(os.environ['MOCK_LATEST']); sys.exit(0)
source=Path(os.environ['MOCK_SERVER']) / args[-1].rsplit('/',1)[-1]
if not source.is_file(): sys.exit(22)
shutil.copyfile(source, args[args.index('--output')+1])
''')
        self.prepare()

    def mock(self, name, contents):
        path = self.bin / name
        path.write_text(contents)
        path.chmod(0o700)

    def prepare(self, target=TARGET, version=VERSION, extra=None, kind=None, inner_bad=False,
                identity_bad=False, duplicate=False, mode=None, root_dir=True):
        linux = 'linux' in target
        names = LINUX_FILES if linux else FILES
        fin = b'''#!/bin/sh
fin_input=no; fin_output=no; fin_error=no
[ -t 0 ] && fin_input=yes
[ -t 1 ] && fin_output=yes
[ -t 2 ] && fin_error=yes
printf 'fin:%s tty:%s:%s:%s\\n' "$*" "$fin_input" "$fin_output" "$fin_error" >> "$MOCK_EVENTS"
exit "${MOCK_SETUP_EXIT:-0}"
'''
        files = {name: b'synthetic public fixture\n' for name in names}
        files['fin'] = fin
        files['BUILDINFO'] = ('source_commit=' + 'a'*40 + '\nsource_state=clean\narchitecture=arm64\nprofile=release\nrelease_version=' + (VERSION if identity_bad else version) + '\ntarget=' + target + '\n').encode()
        files['SHA256SUMS'] = ''.join(checksum(data) + '  ' + name + '\n' for name, data in files.items()).encode()
        if inner_bad:
            files['README.md'] += b'changed after inner hashing'
        asset = 'fin-' + version + '-' + target + ('-discovery' if linux else '') + '.tar.gz'
        archive_path = self.server / asset
        with tarfile.open(archive_path, 'w:gz', format=tarfile.USTAR_FORMAT) as archive:
            if root_dir:
                entry = tarfile.TarInfo('bundle/')
                entry.type = tarfile.DIRTYPE
                entry.mode = 0o755
                archive.addfile(entry)
            for name, data in files.items():
                entry = tarfile.TarInfo('bundle/' + name)
                entry.size = len(data)
                entry.mode = 0o755 if name in ('fin', 'fin-broker', 'fin-touch-id', 'install.sh') else 0o644
                if name == 'fin' and mode is not None: entry.mode = mode
                if name == 'fin' and kind:
                    entry.type = kind
                    entry.linkname = '/tmp/fin-synthetic-link-target'
                    entry.size = 0
                    archive.addfile(entry)
                else:
                    archive.addfile(entry, io.BytesIO(data))
                if name == 'README.md' and duplicate:
                    archive.addfile(entry, io.BytesIO(data))
            if extra:
                entry = tarfile.TarInfo(extra)
                entry.size = 1
                archive.addfile(entry, io.BytesIO(b'x'))
        (self.server / 'SHA256SUMS').write_text(checksum(archive_path.read_bytes()) + '  ' + asset + '\n')
        return archive_path

    def run_installer(self, *args, **env):
        return subprocess.run(['/bin/sh', str(self.script), *args], input=b'', capture_output=True,
                              env=dict(self.env, **env), timeout=15)

    def download(self, *args, **env):
        return self.run_installer('--version', VERSION, '--download-only', str(self.root / 'saved'), *args, **env)

    def assert_refused(self, result):
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertFalse(self.events.exists(), 'No setup or sudo may happen')
        self.assertFalse((self.root / 'saved').exists())

    def terminal(self, args=(), pipe=False, **env):
        master, slave = pty.openpty()
        def session():
            os.setsid()
            fcntl.ioctl(slave, termios.TIOCSCTTY, 0)
        process = subprocess.Popen(['/bin/sh', '-s', '--', *args] if pipe else ['/bin/sh', str(self.script), *args],
                                   stdin=subprocess.PIPE if pipe else slave, stdout=slave, stderr=slave,
                                   env=dict(self.env, **env), preexec_fn=session)
        os.close(slave)
        if pipe:
            process.stdin.write(self.script.read_bytes())
            process.stdin.close()
        text = bytearray()
        end = time.monotonic() + 15
        try:
            while time.monotonic() < end:
                readable, _, _ = select.select([master], [], [], 0.1)
                if readable:
                    try: part = os.read(master, 8192)
                    except OSError: break
                    if not part: break
                    text.extend(part)
                if process.poll() is not None and not readable: break
            process.wait(timeout=2)
        finally:
            if process.poll() is None: process.kill(); process.wait()
            os.close(master)
        return process.returncode, bytes(text)

    def test_help_and_json_never_download_or_request_input(self):
        for args in (('--help',), ('--json',), ('--bad', '--json'), ('--help', '--json')):
            with self.subTest(args=args):
                result = self.run_installer(*args)
                self.assertFalse(self.calls.exists())
                self.assertFalse(self.events.exists())
                self.assertNotIn(b'\x1b', result.stdout + result.stderr)
                if '--json' in args:
                    body = json.loads(result.stdout)
                    self.assertEqual(result.stderr, b'')
                    self.assertTrue('error' in body or 'command' in body)

    def test_invalid_versions_refuse_before_download(self):
        for version in ('v0.2.0\n../bad', '../../bad', 'v0.02.0', 'v0.2.0;touch bad', '', 'latest', 'v1.2', 'v0.2.0-beta.01'):
            with self.subTest(version=version):
                self.assert_refused(self.run_installer('--version', version, '--download-only', str(self.root / 'saved')))
                self.assertFalse(self.calls.exists())

    def test_supported_prerelease_identifiers_match_release_builder(self):
        for version in ('v0.2.0-beta--1', 'v0.2.0-beta-', 'v0.2.0--beta', 'v0.2.0-0'):
            with self.subTest(version=version):
                self.prepare(version=version)
                result=self.run_installer('--version',version,'--download-only',str(self.root/'saved'))
                self.assertEqual(result.returncode,0,result.stderr)
                shutil.rmtree(self.root/'saved')

    def test_arm_and_intel_download_only_keep_all_bundle_files(self):
        for target, machine in ((TARGET, 'arm64'), ('x86_64-apple-darwin', 'x86_64')):
            with self.subTest(target=target):
                self.prepare(target=target)
                result = self.download(MOCK_MACHINE=machine)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(sorted(p.name for p in (self.root/'saved').iterdir()), sorted(FILES + ('SHA256SUMS',)))
                self.assertFalse(self.events.exists())
                shutil.rmtree(self.root/'saved')
        calls = [json.loads(line) for line in self.calls.read_text().splitlines()]
        for call in calls:
            self.assertEqual(call[call.index('--proto')+1], '=https')
            self.assertEqual(call[call.index('--proto-redir')+1], '=https')
            self.assertIn('--tlsv1.2', call)
            self.assertTrue(call[-1].startswith(REPOSITORY + '/releases/download/' + VERSION + '/'))

    def test_latest_resolves_exact_tag_before_asset_urls(self):
        self.prepare(version='v0.2.0')
        result = self.run_installer('--download-only', str(self.root/'saved'), MOCK_LATEST=REPOSITORY+'/releases/tag/v0.2.0')
        self.assertEqual(result.returncode, 0, result.stderr)
        calls = [json.loads(line) for line in self.calls.read_text().splitlines()]
        self.assertIn('--head', calls[0])
        self.assertTrue(all('/releases/download/v0.2.0/' in c[-1] for c in calls[1:]))

    def test_real_release_archive_format_is_accepted_by_downloader(self):
        spec = importlib.util.spec_from_file_location('fin_release', REPO/'scripts/build-release.py')
        release = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(release)
        metadata = {'commit':'a'*40, 'tag':VERSION, 'preview':False}
        for target, system, machine in ((TARGET, 'Darwin', 'arm64'),
                                        ('x86_64-unknown-linux-gnu', 'Linux', 'x86_64')):
            with self.subTest(target=target):
                fixture=self.prepare(target=target)
                with tarfile.open(fixture) as archive:
                    files={member.name.split('/',1)[1]:archive.extractfile(member).read()
                           for member in archive if member.isfile()}
                files['BUILDINFO']=release.buildinfo(metadata,target)
                files.pop('SHA256SUMS')
                files['SHA256SUMS']=release.manifest(files)
                fixture.write_bytes(release.archive_bytes(files))
                release.verify_archive(fixture,metadata,target)
                (self.server/'SHA256SUMS').write_bytes(release.manifest({fixture.name:fixture.read_bytes()}))
                result=self.download(MOCK_SYSTEM=system,MOCK_MACHINE=machine)
                self.assertEqual(result.returncode,0,result.stderr)
                shutil.rmtree(self.root/'saved')
                self.assertFalse(self.events.exists())

    def test_latest_must_resolve_expected_github_repository(self):
        result = self.run_installer('--download-only', str(self.root/'saved'), MOCK_LATEST='https://other.invalid/releases/tag/v0.2.0')
        self.assert_refused(result)
        self.assertEqual(len(self.calls.read_text().splitlines()), 1)

    def test_outer_checksum_mismatch_missing_and_duplicate_refuse(self):
        original = (self.server/'SHA256SUMS').read_text()
        for manifest in ('0'*64 + original[64:], '', original*2):
            with self.subTest(manifest=manifest):
                (self.server/'SHA256SUMS').write_text(manifest)
                self.assert_refused(self.download())

    def test_archive_traversal_unexpected_duplicate_and_links_refuse(self):
        for options in ({'extra':'../outside'}, {'extra':'bundle/unreviewed'}, {'duplicate':True},
                        {'kind':tarfile.SYMTYPE}, {'kind':tarfile.LNKTYPE}, {'root_dir':False}):
            with self.subTest(options=options):
                self.prepare(**options)
                self.assert_refused(self.download())
        self.assertFalse((self.root/'outside').exists())

    def test_inner_checksum_identity_and_nonexecutable_refuse(self):
        for options in ({'inner_bad':True}, {'identity_bad':True, 'version':VERSION+'-other'}, {'mode':0o644}):
            with self.subTest(options=options):
                self.prepare(**options)
                result = self.download() if not options.get('version') else self.run_installer('--version', options['version'], '--download-only', str(self.root/'saved'))
                self.assert_refused(result)

    def test_existing_directory_and_symlink_preserved_without_download(self):
        saved=self.root/'saved'; saved.mkdir(); (saved/'keep').write_text('preserved')
        self.assertNotEqual(self.download().returncode, 0)
        self.assertEqual((saved/'keep').read_text(), 'preserved')
        shutil.rmtree(saved)
        saved.symlink_to(self.root/'missing')
        self.assertNotEqual(self.download().returncode, 0)
        self.assertTrue(saved.is_symlink())
        self.assertFalse(self.calls.exists())

    def test_root_unsupported_and_noninteractive_setup_refuse_without_download(self):
        for env in ({'MOCK_UID':'0'}, {'MOCK_SYSTEM':'Other'}, {'MOCK_MACHINE':'riscv64'}, {}):
            with self.subTest(env=env):
                self.assert_refused(self.run_installer('--version', VERSION, **env))
                self.assertFalse(self.calls.exists())

    def test_linux_discovery_download_has_no_broker_or_installer(self):
        self.prepare(target='x86_64-unknown-linux-gnu')
        result=self.download(MOCK_SYSTEM='Linux', MOCK_MACHINE='x86_64')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(sorted(p.name for p in (self.root/'saved').iterdir()), sorted(LINUX_FILES+('SHA256SUMS',)))
        self.assertFalse(self.events.exists())
        result=self.run_installer('--version', VERSION, MOCK_SYSTEM='Linux', MOCK_MACHINE='x86_64')
        self.assertNotEqual(result.returncode, 0)

    def test_missing_download_refuses_and_removes_private_temporary_files(self):
        self.mock('mktemp', '#!/bin/sh\nmkdir -m 700 "$MOCK_TEMP"\nprintf "%s\\n" "$MOCK_TEMP"\n')
        private=self.root/'temporary'
        result=self.download(MOCK_TEMP=str(private), MOCK_CURL_FAIL='1')
        self.assert_refused(result)
        self.assertFalse(private.exists())

    def test_pipeline_reattaches_terminal_and_exits_without_reading_shell_input(self):
        code, text=self.terminal(('--version', VERSION), pipe=True)
        self.assertEqual(code, 0, text)
        self.assertEqual(self.events.read_text(), 'fin:setup tty:yes:yes:yes\nsudo:-K\n')
        self.assertIn(b'\x1b[32m\xe2\x9c\x93 Verifying bundle contents\x1b[0m', text)
        self.assertNotIn(b'MOCK_', text)

    def test_no_color_and_dumb_have_no_ansi_and_verbose_reaches_setup(self):
        for env in ({'NO_COLOR':''}, {'TERM':'dumb'}):
            with self.subTest(env=env):
                code, text=self.terminal(('--version', VERSION, '--verbose'), **env)
                self.assertEqual(code, 0, text)
                self.assertNotIn(b'\x1b', text)
                self.assertTrue(self.events.read_text().endswith('fin:setup --verbose tty:yes:yes:yes\nsudo:-K\n'))
                self.events.unlink()

    def test_existing_installation_refuses_without_download_or_sudo(self):
        (self.root/'Library/Fin').mkdir(parents=True)
        code, text=self.terminal(('--version', VERSION))
        self.assertNotEqual(code, 0)
        self.assertIn(b'already installed', text)
        self.assertFalse(self.calls.exists())
        self.assertFalse(self.events.exists())

    def test_setup_failure_preserves_exit_code_and_clears_sudo(self):
        code, text=self.terminal(('--version', VERSION), MOCK_SETUP_EXIT='7')
        self.assertEqual(code, 7, text)
        self.assertTrue(self.events.read_text().endswith('sudo:-K\n'))

    def test_cancel_download_and_latest_resolution_reap_child_without_setup(self):
        self.mock('mktemp', '#!/bin/sh\nmkdir -m 700 "$MOCK_TEMP"\nprintf "%s\\n" "$MOCK_TEMP"\n')
        for index, version_args in enumerate((['--version',VERSION], [])):
            with self.subTest(args=version_args):
                private=self.root/('temporary'+str(index))
                child_pid_file=self.root/('curl'+str(index)+'.pid')
                process=subprocess.Popen(['/bin/sh',str(self.script),*version_args,'--download-only',str(self.root/'saved')],
                                         stdout=subprocess.PIPE,stderr=subprocess.PIPE,
                                         env=dict(self.env,MOCK_TEMP=str(private),MOCK_CURL_DELAY='5',MOCK_CURL_PID=str(child_pid_file)))
                try:
                    deadline=time.monotonic()+3
                    while not child_pid_file.exists() and time.monotonic()<deadline: time.sleep(0.01)
                    self.assertTrue(child_pid_file.exists())
                    child_pid=int(child_pid_file.read_text())
                    process.send_signal(signal.SIGTERM)
                    process.communicate(timeout=5)
                    self.assertEqual(process.returncode,143)
                    self.assertFalse(private.exists())
                    self.assertFalse(self.events.exists())
                    with self.assertRaises(ProcessLookupError): os.kill(child_pid,0)
                finally:
                    if process.poll() is None: process.kill(); process.communicate()


if __name__ == '__main__':
    unittest.main()
