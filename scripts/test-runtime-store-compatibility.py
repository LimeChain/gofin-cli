#!/usr/bin/env python3
"""Synthetic tests for the runtime journal-schema downgrade guard."""

import importlib.util
import contextlib
import io
import os
from pathlib import Path
import sqlite3
import stat
import tempfile
import unittest


SCRIPT = Path(__file__).resolve().with_name("check-runtime-store-compatibility.py")
SPEC = importlib.util.spec_from_file_location("runtime_store_compat", SCRIPT)
CHECKER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(CHECKER)


class StoreCompatibilityTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="fin-store-schema-check-")
        self.addCleanup(self.temporary.cleanup)
        self.directory = Path(self.temporary.name) / "private"
        self.directory.mkdir(mode=0o700)
        self.database = self.directory / "operations.sqlite"
        self.expected_uid = os.getuid()
        with sqlite3.connect(self.database) as connection:
            connection.execute("PRAGMA user_version=9")
            connection.execute("CREATE TABLE synthetic_record (secret TEXT)")
            connection.execute("INSERT INTO synthetic_record VALUES ('must not be read')")
        self.database.chmod(0o600)

    def check(self, supported):
        return CHECKER.check_compatibility(
            self.database, supported, expected_uid=self.expected_uid
        )

    def test_rejects_newer_journal_schema_without_changing_any_bytes(self):
        before = self.database.read_bytes()
        with self.assertRaisesRegex(RuntimeError, r"journal schema 9.*broker schema 4"):
            self.check(4)
        self.assertEqual(self.database.read_bytes(), before)

    def test_accepts_matching_schema(self):
        self.assertEqual(self.check(9), 9)

    def test_active_wal_schema_is_read_and_wal_is_preserved(self):
        connection = sqlite3.connect(self.database)
        try:
            connection.execute("PRAGMA journal_mode=WAL")
            connection.execute("PRAGMA wal_autocheckpoint=0")
            connection.execute("PRAGMA user_version=10")
            connection.commit()
            wal = Path(str(self.database) + "-wal")
            self.assertTrue(wal.exists())
            self.assertGreater(wal.stat().st_size, 0)
            wal_before = wal.read_bytes()
            with self.assertRaisesRegex(RuntimeError, r"journal schema 10.*broker schema 9"):
                self.check(9)
            self.assertEqual(wal.read_bytes(), wal_before)
        finally:
            connection.close()

    def test_fails_closed_on_symlink_and_unsafe_metadata(self):
        original = self.database
        linked = self.directory / "linked.sqlite"
        linked.symlink_to(original)
        with self.assertRaisesRegex(RuntimeError, "unsafe file type"):
            CHECKER.check_compatibility(linked, 9, expected_uid=self.expected_uid)
        shm = Path(str(self.database) + "-shm")
        shm.symlink_to(original)
        with self.assertRaisesRegex(RuntimeError, "sidecar has unsafe ownership or permissions"):
            self.check(9)
        shm.unlink()
        self.directory.chmod(0o770)
        with self.assertRaisesRegex(RuntimeError, "unsafe ownership or permissions"):
            self.check(9)
        self.directory.chmod(0o700)
        self.database.chmod(0o666)
        with self.assertRaisesRegex(RuntimeError, "unsafe ownership or permissions"):
            self.check(9)

    def test_fails_closed_on_unreadable_or_invalid_database_metadata(self):
        missing = self.directory / "missing.sqlite"
        with self.assertRaisesRegex(RuntimeError, "metadata is unavailable"):
            CHECKER.check_compatibility(missing, 9, expected_uid=self.expected_uid)
        self.database.write_bytes(b"not a sqlite database")
        self.database.chmod(0o600)
        with self.assertRaisesRegex(RuntimeError, "could not be read safely"):
            self.check(9)

    def test_incoming_schema_must_be_positive_integer(self):
        for value in (0, -1, True, "9"):
            with self.subTest(value=value), self.assertRaisesRegex(RuntimeError, "invalid journal schema"):
                CHECKER.check_compatibility(self.database, value, expected_uid=self.expected_uid)

    def test_command_line_does_not_accept_a_journal_path_override(self):
        with contextlib.redirect_stderr(io.StringIO()):
            with self.assertRaises(SystemExit) as raised:
                CHECKER.main(["--supported-schema", "9", "--journal", str(self.database)])
        self.assertEqual(raised.exception.code, 2)


if __name__ == "__main__":
    unittest.main(verbosity=2)
