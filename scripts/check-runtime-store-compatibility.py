#!/usr/bin/env python3
"""Fail-closed, metadata-only guard against downgrading GoFin's journal schema.

The command-line interface intentionally has no journal path option. The
path-injected function is used only by isolated synthetic tests.
"""

import argparse
import os
from pathlib import Path
import sqlite3
import stat
import sys
from urllib.parse import quote


JOURNAL = Path("/Library/Fin/private/operations.sqlite")


def _safe_root_owned(path, *, expected_uid):
    try:
        info = os.lstat(path)
    except OSError as error:
        raise RuntimeError("protected journal metadata is unavailable") from error
    if stat.S_ISLNK(info.st_mode) or not stat.S_ISREG(info.st_mode):
        raise RuntimeError("protected journal metadata has an unsafe file type")
    if info.st_uid != expected_uid or info.st_mode & (stat.S_IWGRP | stat.S_IWOTH):
        raise RuntimeError("protected journal metadata has unsafe ownership or permissions")
    return info


def _validate_paths(journal, *, expected_uid):
    try:
        parent = os.lstat(journal.parent)
    except OSError as error:
        raise RuntimeError("protected journal directory is unavailable") from error
    if (stat.S_ISLNK(parent.st_mode) or not stat.S_ISDIR(parent.st_mode)
            or parent.st_uid != expected_uid
            or parent.st_mode & (stat.S_IWGRP | stat.S_IWOTH)):
        raise RuntimeError("protected journal directory has unsafe ownership or permissions")
    _safe_root_owned(journal, expected_uid=expected_uid)
    for suffix in ("-wal", "-shm"):
        sidecar = Path(str(journal) + suffix)
        if not os.path.lexists(sidecar):
            continue
        try:
            sidecar_info = os.lstat(sidecar)
        except OSError as error:
            raise RuntimeError("protected journal sidecar metadata is unavailable") from error
        if (stat.S_ISLNK(sidecar_info.st_mode) or not stat.S_ISREG(sidecar_info.st_mode)
                or sidecar_info.st_uid != expected_uid
                or sidecar_info.st_mode & (stat.S_IWGRP | stat.S_IWOTH)):
            raise RuntimeError("protected journal sidecar has unsafe ownership or permissions")


def check_compatibility(journal, supported_schema, *, expected_uid):
    """Return journal schema when it is supported; inspect no journal records."""
    if isinstance(supported_schema, bool) or not isinstance(supported_schema, int) or supported_schema <= 0:
        raise RuntimeError("incoming broker reported an invalid journal schema version")
    journal = Path(journal)
    _validate_paths(journal, expected_uid=expected_uid)

    wal = Path(str(journal) + "-wal")
    try:
        # immutable=1 avoids SQLite creating sidecars when no live WAL needs to
        # participate. With a non-empty WAL, ordinary read-only mode is needed
        # so user_version reflects the current committed schema.
        immutable = not (wal.exists() and wal.stat().st_size > 0)
        uri = "file:" + quote(str(journal), safe="/") + "?mode=ro"
        if immutable:
            uri += "&immutable=1"
        connection = sqlite3.connect(uri, uri=True, timeout=1.0)
        try:
            row = connection.execute("PRAGMA user_version").fetchone()
        finally:
            connection.close()
    except (OSError, sqlite3.Error) as error:
        raise RuntimeError("protected journal schema could not be read safely") from error
    if not row or isinstance(row[0], bool) or not isinstance(row[0], int) or row[0] < 0:
        raise RuntimeError("protected journal schema metadata is invalid")
    journal_schema = row[0]
    if journal_schema > supported_schema:
        raise RuntimeError(
            f"installed journal schema {journal_schema} is newer than incoming broker schema "
            f"{supported_schema}; update refused to preserve financial records"
        )
    return journal_schema


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--supported-schema", required=True, type=int)
    args = parser.parse_args(argv)
    if os.geteuid() != 0:
        print("Run this metadata check from the reviewed root updater.", file=sys.stderr)
        return 1
    try:
        version = check_compatibility(JOURNAL, args.supported_schema, expected_uid=0)
    except RuntimeError as error:
        print(str(error), file=sys.stderr)
        return 1
    print(f"Journal schema {version} is compatible with incoming broker schema {args.supported_schema}.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
