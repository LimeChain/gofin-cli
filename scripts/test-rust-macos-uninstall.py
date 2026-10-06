#!/usr/bin/env python3
"""Isolated reset fixtures: no sudo, real installation, credentials or service calls."""
import fcntl
from contextlib import closing
import importlib.util
import os
from pathlib import Path
import plistlib
import sqlite3
import subprocess
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('fin_reset', Path(__file__).with_name('uninstall-rust-macos.py'))
reset = importlib.util.module_from_spec(spec)
spec.loader.exec_module(reset)


class ResetTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.base = Path(self.tmp.name).resolve()
        self.root = self.base / 'Fin'
        (self.root / 'private/credentials').mkdir(parents=True)
        (self.root / 'runtime').mkdir()
        (self.root / 'private/credentials/synthetic').write_text('SYNTHETIC TEST KEY ONLY')
        (self.root / 'private/config.json').write_text('synthetic config')
        (self.root / 'private/maintenance.lock').touch()
        (self.root / 'private/completion-owner').write_text('501\n')
        (self.root / 'private/completion-owner').chmod(0o600)
        (self.root / 'runtime/fin').write_text('synthetic executable')
        (self.root / 'runtime/fin').chmod(0o755)
        self.plist = self.base / 'service.plist'
        self.plist.write_bytes(plistlib.dumps({'Label':'io.fin-cli.broker','ProgramArguments':[str(self.root/'runtime/fin-broker')],
                                               'Sockets': {'Broker': {'SockPathName': '/Library/Fin/run/broker.sock', 'SockPathOwner': 501}}}))
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
            code = 0 if self.active or self.active_process_name in ('fin', 'fin-touch-id', 'Fin') else 1
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
        self.assertTrue(path.is_relative_to(self.base))
        self.assertFalse(path.is_symlink())
        self.assertTrue(path.is_dir() if directory else path.is_file())

    def execute(self):
        with patch.object(reset, 'protected', self.check_fixture_path):
            return reset.uninstall(self.root, self.plist, self.registration, self.run_command)

    def test_clean_reset_deletes_secrets_config_service_path_without_backup(self):
        self.execute()
        self.assertEqual(list(self.base.iterdir()), [])
        self.assertTrue(any('bootout' in command for command in self.calls))
        completion = next(command for command in self.calls if command[0] == '/usr/bin/sudo')
        self.assertEqual(completion, ['/usr/bin/sudo', '-n', '-H', '-u', '#501',
                                      str(self.root / 'runtime/fin'), 'internal-completions', 'remove', '--json'])

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
        self.plist.write_bytes(plistlib.dumps({'Label':'io.fin-cli.broker',
                                               'ProgramArguments':[str(self.root/'runtime/fin-broker')],
                                               'Sockets': {'Broker': {'SockPathName': '/Library/Fin/run/broker.sock', 'SockPathOwner': 777}}}))
        (self.root / 'private/completion-owner').write_text('777\n')
        self.execute()
        completion = next(command for command in self.calls if command[0] == '/usr/bin/sudo')
        self.assertEqual(completion[4], '#777')

    def test_invalid_completion_owner_refuses_before_commands(self):
        self.plist.write_bytes(plistlib.dumps({'Label':'io.fin-cli.broker',
                                               'ProgramArguments':[str(self.root/'runtime/fin-broker')],
                                               'Sockets': {'Broker': {'SockPathName': '/Library/Fin/run/broker.sock', 'SockPathOwner': True}}}))
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
        self.active_process_name = 'Fin'
        with self.assertRaisesRegex(reset.ResetError, 'command or approval is active'):
            self.execute()
        self.assertTrue(self.root.exists())
        self.assertEqual(self.calls[0][-1], 'fin|fin-touch-id|Fin')

    def test_completion_helper_requires_protected_executable_metadata(self):
        (self.root / 'runtime/fin').chmod(0o644)
        with self.assertRaisesRegex(reset.ResetError, 'not executable'):
            self.execute()
        self.assertEqual(self.calls, [])

    def test_every_financial_table_blocks_deletion_and_restores_service(self):
        for table in ('operations','runs','submissions','reservations','effect_ledger','proof_records'):
            with self.subTest(table=table):
                with closing(sqlite3.connect(self.database)) as db, db:
                    db.execute('INSERT INTO ' + table + ' VALUES (?)', ('synthetic-unresolved',))
                with self.assertRaisesRegex(reset.ResetError, 'Financial history'):
                    self.execute()
                self.assertTrue((self.root / 'private/credentials/synthetic').exists())
                self.assertTrue(any('bootstrap' in command for command in self.calls))
                with closing(sqlite3.connect(self.database)) as db, db:
                    db.execute('DELETE FROM ' + table)

    def test_unreadable_schema_refuses(self):
        with closing(sqlite3.connect(self.database)) as db, db:
            db.execute('DROP TABLE operations')
        with self.assertRaisesRegex(reset.ResetError, 'journal'):
            self.execute()
        self.assertTrue(self.root.exists())

    def test_sqlite_diagnostics_are_specific_without_echoing_arbitrary_data(self):
        for message, expected in [
            ('database is locked', 'database is locked'),
            ('disk I/O error', 'SQLite disk I/O failure'),
            ('no such table: operations', 'missing expected operations table'),
            ('SYNTHETIC_PRIVATE_DATA_NOT_FOR_OUTPUT', 'unclassified SQLite failure'),
        ]:
            self.assertEqual(reset.journal_error_reason(sqlite3.OperationalError(message)), expected)

    def test_wal_history_is_not_mistaken_for_an_empty_database(self):
        with closing(sqlite3.connect(self.database)) as writer:
            writer.execute('PRAGMA journal_mode=WAL')
            writer.execute("INSERT INTO operations VALUES ('synthetic-wal-only-record')")
            writer.commit()
            self.assertTrue(Path(str(self.database) + '-wal').exists())
            with self.assertRaisesRegex(reset.ResetError, 'Financial history'):
                self.execute()
            self.assertTrue(self.root.exists())

    def test_snapshot_handles_read_only_wal_support_failure_and_preserves_original(self):
        # Reproduce the reported error using macOS SQLite: readonly WAL cannot
        # create its missing shm in a non-writable source directory.
        source = self.base / 'source.sqlite'
        with closing(sqlite3.connect(source)) as writer:
            writer.execute('PRAGMA journal_mode=WAL')
            for table in ('operations','runs','submissions','reservations','effect_ledger','proof_records'):
                writer.execute('CREATE TABLE ' + table + ' (id TEXT)')
            writer.commit()
            import shutil
            shutil.copyfile(source, self.database)
            wal = Path(str(self.database) + '-wal')
            shutil.copyfile(Path(str(source) + '-wal'), wal)
            before = (self.database.read_bytes(), wal.read_bytes())
            self.database.parent.chmod(0o500)
            try:
                if os.geteuid() != 0:
                    with closing(sqlite3.connect(self.database.as_uri() + '?mode=ro', uri=True)) as reader:
                        with self.assertRaises(sqlite3.OperationalError):
                            reader.execute('SELECT * FROM operations').fetchall()
                with patch.object(reset, 'protected', self.check_fixture_path):
                    reset.empty_journal(self.database)
                self.assertEqual(before, (self.database.read_bytes(), wal.read_bytes()))
                self.assertFalse(list(self.root.glob('.fin-journal-check-*')))
            finally:
                self.database.parent.chmod(0o700)

    def test_journal_is_checked_after_stop_and_before_deletion(self):
        original = reset.empty_journal
        def inspect(path):
            self.assertTrue(any('bootout' in command for command in self.calls))
            self.assertTrue(self.plist.exists())
            self.assertTrue((self.root / 'private/credentials/synthetic').exists())
            return original(path)
        with patch.object(reset, 'empty_journal', side_effect=inspect):
            self.execute()

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

    def test_symlinks_fail_real_path_guard(self):
        link = self.base / 'link'
        link.symlink_to(self.root)
        with self.assertRaises(reset.ResetError):
            reset.protected(link, True)
        self.assertTrue(self.root.exists())


if __name__ == '__main__':
    unittest.main()
