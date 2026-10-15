#!/usr/bin/env python3
"""Isolated reset fixtures: no sudo, real installation, credentials or service calls."""
import fcntl
from contextlib import closing
import importlib.util
import io
from pathlib import Path
import plistlib
import sqlite3
import subprocess
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('gofin_reset', Path(__file__).with_name('uninstall-rust-macos.py'))
reset = importlib.util.module_from_spec(spec)
spec.loader.exec_module(reset)


class ResetTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.base = Path(self.tmp.name).resolve()
        self.root = self.base / 'GoFin'
        (self.root / 'private/credentials').mkdir(parents=True)
        (self.root / 'runtime').mkdir()
        (self.root / 'private/credentials/synthetic').write_text('SYNTHETIC TEST KEY ONLY')
        (self.root / 'private/config.json').write_text('synthetic config')
        (self.root / 'private/maintenance.lock').touch()
        (self.root / 'private/completion-owner').write_text('501\n')
        (self.root / 'private/completion-owner').chmod(0o600)
        (self.root / 'runtime/gofin').write_text('synthetic executable')
        (self.root / 'runtime/gofin').chmod(0o755)
        self.plist = self.base / 'service.plist'
        self.plist.write_bytes(plistlib.dumps({'Label':'io.gofin-cli.broker','ProgramArguments':[str(self.root/'runtime/gofin-broker')],
                                               'Sockets': {'Broker': {'SockPathName': '/Library/GoFin/run/broker.sock', 'SockPathOwner': 501}}}))
        self.registration = self.base / 'path'
        self.registration.write_text(str(self.root / 'runtime') + '\n')
        self.database = self.root / 'private/operations.sqlite'
        with closing(sqlite3.connect(self.database)) as db, db:
            for table in ('operations','runs','submissions','reservations','effect_ledger','proof_records'):
                db.execute('CREATE TABLE ' + table + ' (id TEXT)')
        self.calls = []
        self.active = False
        self.active_process_name = None
        self.fail_stop = False
        self.fail_completions = False
        self.timeout_completions = False
        self.completion_status = 'removed'
        self.completion_json = True

    def run_command(self, arguments, **kwargs):
        self.calls.append(arguments)
        if arguments[0].endswith('pgrep'):
            code = 0 if self.active or self.active_process_name in ('fin', 'fin-touch-id', 'Fin', 'gofin', 'gofin-touch-id', 'GoFin') else 1
        elif arguments[0] == '/usr/bin/sudo':
            if self.timeout_completions:
                raise subprocess.TimeoutExpired(arguments, 15)
            code = 1 if self.fail_completions else 0
            if code == 0:
                if self.completion_json:
                    return subprocess.CompletedProcess(arguments, 0, stdout=(
                        '{"status":"' + self.completion_status + '","shell":"zsh",'
                        '"message":"synthetic safe status","warnings":[]}').encode())
                return subprocess.CompletedProcess(arguments, 0, stdout=b'not-json')
        elif 'bootout' in arguments:
            code = 1 if self.fail_stop else 0
        else:
            code = 0
        return subprocess.CompletedProcess(arguments, code)

    def check_fixture_path(self, path, directory=False):
        if not path.is_relative_to(self.base) or path.is_symlink():
            raise reset.ResetError('Unexpected synthetic path.')
        if not (path.is_dir() if directory else path.is_file()):
            raise reset.ResetError('Unexpected synthetic path type.')

    def execute(self, confirm=lambda: True):
        with patch.object(reset, 'protected', self.check_fixture_path):
            return reset.uninstall(self.root, self.plist, self.registration, self.run_command, confirm)

    def test_clean_reset_deletes_secrets_config_service_path_without_backup(self):
        self.execute()
        self.assertEqual(list(self.base.iterdir()), [])
        self.assertTrue(any('bootout' in command for command in self.calls))
        completion = next(command for command in self.calls if command[0] == '/usr/bin/sudo')
        self.assertEqual(completion, ['/usr/bin/sudo', '-n', '-H', '-u', '#501',
                                      str(self.root / 'runtime/gofin'), 'internal-completions', 'remove', '--json'])

    def test_legacy_fin_installation_uses_its_existing_cli_for_completion_removal(self):
        (self.root / 'runtime/gofin').rename(self.root / 'runtime/fin')
        service = plistlib.loads(self.plist.read_bytes())
        service['ProgramArguments'] = [str(self.root / 'runtime/fin-broker')]
        self.plist.write_bytes(plistlib.dumps(service))
        self.assertEqual(self.execute(), 'removed')
        self.assertFalse(self.root.exists())
        completion = next(command for command in self.calls if command[0] == '/usr/bin/sudo')
        self.assertEqual(completion[5], str(self.root / 'runtime/fin'))

    def test_legacy_touch_id_process_blocks_uninstall(self):
        self.active_process_name = 'Fin'
        with self.assertRaisesRegex(reset.ResetError, 'command or approval is active'):
            self.execute()
        self.assertTrue(self.root.exists())

    def test_legacy_root_uninstalls_with_either_existing_cli_generation(self):
        legacy_root = self.base / 'Fin'
        self.root.rename(legacy_root)
        self.root = legacy_root
        (self.root / 'runtime/gofin').rename(self.root / 'runtime/fin')
        service = plistlib.loads(self.plist.read_bytes())
        service['Label'] = 'io.fin-cli.broker'
        service['ProgramArguments'] = [str(self.root / 'runtime/fin-broker')]
        service['Sockets']['Broker']['SockPathName'] = '/Library/Fin/run/broker.sock'
        self.plist.write_bytes(plistlib.dumps(service))
        self.registration.write_text(str(self.root / 'runtime') + '\n')
        self.assertEqual(self.execute(), 'removed')
        self.assertFalse(self.root.exists())
        self.assertTrue(any(command == ['/bin/launchctl', 'bootout', 'system/io.fin-cli.broker'] for command in self.calls))

    def test_installation_detection_refuses_ambiguous_old_and_new_roots(self):
        legacy = self.base / 'Fin'
        legacy.mkdir()
        with patch.object(reset, 'ROOT', self.root), patch.object(reset, 'PLIST', self.plist), patch.object(reset, 'REGISTRATION', self.registration), patch.object(reset, 'LEGACY_ROOT', legacy), patch.object(reset, 'LEGACY_PLIST', self.base / 'old.plist'), patch.object(reset, 'LEGACY_REGISTRATION', self.base / 'old.path'):
            with self.assertRaisesRegex(reset.ResetError, 'Both Fin and GoFin'):
                reset.installation_paths()
        self.assertTrue(self.root.exists())
        self.assertTrue(legacy.exists())

    def test_installation_detection_selects_exact_legacy_namespace(self):
        legacy_root = self.base / 'Fin'
        self.root.rename(legacy_root)
        old_plist = self.base / 'old.plist'
        old_path = self.base / 'old.path'
        self.plist.rename(old_plist)
        self.registration.rename(old_path)
        with patch.object(reset, 'ROOT', self.root), patch.object(reset, 'PLIST', self.plist), patch.object(reset, 'REGISTRATION', self.registration), patch.object(reset, 'LEGACY_ROOT', legacy_root), patch.object(reset, 'LEGACY_PLIST', old_plist), patch.object(reset, 'LEGACY_REGISTRATION', old_path):
            self.assertEqual(reset.installation_paths(), (legacy_root, old_plist, old_path))

    def test_completion_cleanup_failure_preserves_installation_and_restarts_service(self):
        self.fail_completions = True
        with self.assertRaisesRegex(reset.ResetError, 'Could not remove shell completions'):
            self.execute()
        self.assertTrue((self.root / 'private/credentials/synthetic').exists())
        self.assertTrue(self.plist.exists())
        self.assertTrue(self.registration.exists())
        self.assertTrue(any('bootstrap' in command for command in self.calls))

    def test_partial_completion_cleanup_is_reported_as_safe_status(self):
        self.completion_status = 'partial'
        self.assertEqual(self.execute(), 'partial')
        self.assertFalse(self.root.exists())

    def test_old_install_without_completion_manifest_is_noop(self):
        (self.root / 'private/completion-owner').unlink()
        self.completion_status = 'absent'
        self.assertEqual(self.execute(), 'absent')
        self.assertFalse(self.root.exists())
        self.assertFalse(any(command[0] == '/usr/bin/sudo' for command in self.calls))

    def test_invalid_completion_json_preserves_installation_and_restarts_service(self):
        self.completion_json = False
        with self.assertRaisesRegex(reset.ResetError, 'Could not remove shell completions'):
            self.execute()
        self.assertTrue(self.root.exists())
        self.assertTrue(any('bootstrap' in command for command in self.calls))

    def test_completion_timeout_preserves_installation_and_restarts_service(self):
        self.timeout_completions = True
        with self.assertRaisesRegex(reset.ResetError, 'Could not remove shell completions'):
            self.execute()
        self.assertTrue(self.root.exists())
        self.assertTrue(any('bootstrap' in command for command in self.calls))

    def test_completion_cleanup_uses_trusted_original_owner_not_sudo_uid(self):
        self.plist.write_bytes(plistlib.dumps({'Label':'io.gofin-cli.broker',
                                               'ProgramArguments':[str(self.root/'runtime/gofin-broker')],
                                               'Sockets': {'Broker': {'SockPathName': '/Library/GoFin/run/broker.sock', 'SockPathOwner': 777}}}))
        (self.root / 'private/completion-owner').write_text('777\n')
        self.execute()
        completion = next(command for command in self.calls if command[0] == '/usr/bin/sudo')
        self.assertEqual(completion[4], '#777')

    def test_invalid_completion_owner_refuses_before_commands(self):
        self.plist.write_bytes(plistlib.dumps({'Label':'io.gofin-cli.broker',
                                               'ProgramArguments':[str(self.root/'runtime/gofin-broker')],
                                               'Sockets': {'Broker': {'SockPathName': '/Library/GoFin/run/broker.sock', 'SockPathOwner': True}}}))
        with self.assertRaisesRegex(reset.ResetError, 'valid original owner'):
            self.execute()
        self.assertEqual(self.calls, [])

    def test_completion_marker_must_have_private_mode(self):
        marker = self.root / 'private/completion-owner'
        marker.chmod(0o644)
        with self.assertRaisesRegex(reset.ResetError, 'marker permissions'):
            self.execute()
        self.assertEqual(self.calls, [])

    def test_completion_marker_uid_must_match_plist_owner(self):
        (self.root / 'private/completion-owner').write_text('777\n')
        with self.assertRaisesRegex(reset.ResetError, 'does not match'):
            self.execute()
        self.assertEqual(self.calls, [])

    def test_human_named_touch_id_process_blocks_uninstall(self):
        self.active_process_name = 'GoFin'
        with self.assertRaisesRegex(reset.ResetError, 'command or approval is active'):
            self.execute()
        self.assertTrue(self.root.exists())
        self.assertEqual(self.calls[0][-1], 'fin|fin-touch-id|Fin|gofin|gofin-touch-id|GoFin')

    def test_completion_helper_requires_protected_executable_metadata(self):
        (self.root / 'runtime/gofin').chmod(0o644)
        with self.assertRaisesRegex(reset.ResetError, 'not executable'):
            self.execute()
        self.assertEqual(self.calls, [])

    def invoke_main(self, answer):
        output, prompt = io.StringIO(), io.StringIO()
        with patch.object(reset, 'protected', self.check_fixture_path), \
                patch.object(reset.sys, 'platform', 'darwin'), \
                patch.object(reset.os, 'geteuid', return_value=0), \
                patch.object(reset.os, 'isatty', return_value=True), \
                patch.dict(reset.os.environ, {'SUDO_UID':'501'}):
            result = reset.main(
                self.root, self.plist, self.registration, self.run_command,
                input_stream=answer, prompt_stream=prompt, result_stream=output,
            )
        return result, output.getvalue(), prompt.getvalue()

    def test_confirmed_main_deletes_populated_pending_history_and_wal(self):
        reader = sqlite3.connect(self.database)
        try:
            reader.execute('PRAGMA journal_mode=WAL')
            reader.execute('BEGIN')
            reader.execute('SELECT * FROM operations').fetchall()
            with closing(sqlite3.connect(self.database)) as writer, writer:
                writer.execute("INSERT INTO operations VALUES ('synthetic-pending-recovery-record')")
            wal = Path(str(self.database) + '-wal')
            self.assertTrue(wal.exists())
            status, output, prompt = self.invoke_main(io.StringIO('yes\n'))
            self.assertEqual(status, 0)
            self.assertIn('financial history deleted', output)
            self.assertIn('pending', prompt)
            self.assertIn('avoiding duplicates', prompt)
            self.assertIn('does not cancel existing', prompt)
            self.assertIn('exchanges or chains', prompt)
            self.assertTrue(any('bootout' in command for command in self.calls))
            self.assertFalse(self.root.exists())
            self.assertFalse(wal.exists())
        finally:
            reader.close()

    def test_missing_database_does_not_block_confirmed_uninstall(self):
        self.database.unlink()
        status, output, _ = self.invoke_main(io.StringIO('yes\n'))
        self.assertEqual(status, 0)
        self.assertIn('financial history deleted', output)
        self.assertFalse(self.root.exists())

    def test_damaged_database_does_not_block_confirmed_uninstall(self):
        self.database.write_bytes(b'not sqlite data')
        status, output, _ = self.invoke_main(io.StringIO('yes\n'))
        self.assertEqual(status, 0)
        self.assertIn('financial history deleted', output)
        self.assertFalse(self.root.exists())

    def test_decline_eof_and_interrupt_preserve_all_files_and_service(self):
        class InterruptedInput:
            def readline(self):
                raise KeyboardInterrupt

        with closing(sqlite3.connect(self.database)) as db, db:
            db.execute("INSERT INTO operations VALUES ('synthetic-pending-recovery-record')")
        wal = Path(str(self.database) + '-wal')
        shm = Path(str(self.database) + '-shm')
        wal.write_bytes(b'SYNTHETIC PENDING WAL')
        shm.write_bytes(b'SYNTHETIC SHARED MEMORY')
        for label, answer, expected_status in (
            ('declined', io.StringIO('n\n'), 1),
            ('enter-default-no', io.StringIO('\n'), 1),
            ('eof', io.StringIO(''), 1),
            ('interrupt', InterruptedInput(), 130),
        ):
            with self.subTest(label=label):
                before = {path.relative_to(self.base): path.read_bytes()
                          for path in self.base.rglob('*') if path.is_file()}
                status, output, prompt = self.invoke_main(answer)
                self.assertEqual(status, expected_status)
                self.assertIn('saved data were not changed', output + prompt)
                self.assertIn('financial history', prompt)
                after = {path.relative_to(self.base): path.read_bytes()
                         for path in self.base.rglob('*') if path.is_file()}
                self.assertEqual(after, before)
                self.assertFalse(any('bootout' in command for command in self.calls))
                self.assertFalse(any(command[0] == '/usr/bin/sudo' for command in self.calls))
                self.calls.clear()

    def test_changed_path_registration_refuses(self):
        self.registration.write_text('/unrelated\n')
        with self.assertRaisesRegex(reset.ResetError, 'PATH'):
            self.execute()
        self.assertTrue(self.root.exists())

    def test_wrong_service_refuses(self):
        self.plist.write_bytes(plistlib.dumps({'Label':'another-service'}))
        with self.assertRaisesRegex(reset.ResetError, 'Service'):
            self.execute()
        self.assertEqual(self.calls, [])

    def test_active_client_refuses(self):
        self.active = True
        with self.assertRaisesRegex(reset.ResetError, 'active'):
            self.execute()
        self.assertTrue(self.root.exists())

    def test_busy_maintenance_lock_refuses(self):
        with (self.root / 'private/maintenance.lock').open() as lock:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            with self.assertRaisesRegex(reset.ResetError, 'busy'):
                self.execute()
        self.assertEqual(self.calls, [])

    def test_failed_service_stop_preserves_credentials(self):
        self.fail_stop = True
        with self.assertRaisesRegex(reset.ResetError, 'stop'):
            self.execute()
        self.assertTrue((self.root / 'private/credentials/synthetic').exists())

    def test_financial_storage_symlink_fails_path_guard_before_commands(self):
        self.database.unlink()
        outside = self.base / 'outside.sqlite'
        outside.write_bytes(b'SYNTHETIC OUTSIDE DATA')
        self.database.symlink_to(outside)
        with self.assertRaises(reset.ResetError):
            self.execute()
        self.assertEqual(self.calls, [])
        self.assertEqual(outside.read_bytes(), b'SYNTHETIC OUTSIDE DATA')

    def test_symlinks_fail_real_path_guard(self):
        link = self.base / 'link'
        link.symlink_to(self.root)
        with self.assertRaises(reset.ResetError):
            reset.protected(link, True)
        self.assertTrue(self.root.exists())


if __name__ == '__main__':
    unittest.main()
