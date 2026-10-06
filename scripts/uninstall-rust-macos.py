#!/usr/bin/python3
"""Human-only local beta reset. No credential backup, network or financial calls."""
import fcntl
import json
import os
from pathlib import Path
import plistlib
import shutil
import sqlite3
import stat
import subprocess
import sys
import tempfile
import time

ROOT = Path('/Library/Fin')
PLIST = Path('/Library/LaunchDaemons/io.fin-cli.broker.plist')
REGISTRATION = Path('/private/etc/paths.d/fin')


class ResetError(Exception):
    pass


def protected(path, directory=False):
    metadata = path.lstat()
    expected = stat.S_ISDIR if directory else stat.S_ISREG
    if not expected(metadata.st_mode) or metadata.st_uid != 0 or metadata.st_mode & 0o022:
        raise ResetError('Unexpected installation ownership or path type; nothing was deleted.')


def journal_error_reason(error):
    # macOS system Python 3.9 does not expose sqlite_errorcode/name. Translate
    # only known fixed messages; never print arbitrary database error contents.
    message = str(error).lower()
    reasons = {
        'database is locked': 'database is locked',
        'database table is locked': 'database table is locked',
        'database is busy': 'database is busy',
        'unable to open database file': 'database could not be opened',
        'attempt to write a readonly database': 'SQLite needs write access for recovery',
        'disk i/o error': 'SQLite disk I/O failure',
        'file is not a database': 'file is not a SQLite database',
        'database disk image is malformed': 'database is damaged',
    }
    for table in ('operations', 'runs', 'submissions', 'reservations', 'effect_ledger', 'proof_records'):
        if message == 'no such table: ' + table:
            return 'missing expected ' + table + ' table'
    return reasons.get(message, 'unclassified SQLite failure')


def empty_journal(path):
    # Caller holds the maintenance lock and has stopped the broker. Snapshot the
    # DB AND its WAL, never just the main file: committed rows may live only in
    # WAL. Let SQLite rebuild support files in this private disposable snapshot.
    protected(path)
    wal = path.with_name(path.name + '-wal')
    has_wal = wal.exists() or wal.is_symlink()
    if has_wal:
        protected(wal)
    stage = 'opening snapshot'
    try:
        with tempfile.TemporaryDirectory(prefix='.fin-journal-check-', dir=path.parent.parent) as directory:
            snapshot = Path(directory) / 'journal.sqlite'
            shutil.copyfile(path, snapshot)
            snapshot.chmod(0o600)
            if has_wal:
                snapshot_wal = snapshot.with_name(snapshot.name + '-wal')
                shutil.copyfile(wal, snapshot_wal)
                snapshot_wal.chmod(0o600)
            connection = sqlite3.connect(snapshot.as_uri() + '?mode=rw', uri=True)
            try:
                # Original DB is never opened by SQLite, checkpointed or modified.
                for table in ('operations', 'runs', 'submissions', 'reservations', 'effect_ledger', 'proof_records'):
                    stage = 'checking ' + table
                    if connection.execute('SELECT EXISTS(SELECT 1 FROM ' + table + ' LIMIT 1)').fetchone()[0]:
                        raise ResetError('Financial history exists. Reset stopped; credentials and journal are intact. Resolve or preserve that history before resetting.')
            finally:
                connection.close()
    except sqlite3.Error as error:
        raise ResetError('Could not verify the financial journal: ' + journal_error_reason(error) + ' (' + stage + '). Nothing was deleted.') from None


def uninstall(root=ROOT, plist=PLIST, registration=REGISTRATION, run=subprocess.run):
    protected(root, True)
    protected(root / 'private', True)
    protected(root / 'private/maintenance.lock')
    protected(plist)
    protected(registration)
    if registration.read_text() != str(root / 'runtime') + '\n':
        raise ResetError('PATH registration differs from this installation; nothing was deleted.')
    try:
        service = plistlib.loads(plist.read_bytes())
    except Exception:
        raise ResetError('Service registration is invalid; nothing was deleted.') from None
    if service.get('Label') != 'io.fin-cli.broker' or service.get('ProgramArguments') != [str(root / 'runtime/fin-broker')]:
        raise ResetError('Service registration differs from this installation; nothing was deleted.')
    sockets = service.get('Sockets')
    broker_socket = sockets.get('Broker') if isinstance(sockets, dict) else None
    if not isinstance(broker_socket, dict) or broker_socket.get('SockPathName') != '/Library/Fin/run/broker.sock':
        raise ResetError('Service socket registration differs from this installation; nothing was deleted.')
    original_owner = broker_socket.get('SockPathOwner')
    if type(original_owner) is not int or not 501 <= original_owner < 2**32:
        raise ResetError('Service registration has no valid original owner; nothing was deleted.')
    completion_marker = root / 'private/completion-owner'
    completion_enabled = completion_marker.exists() or completion_marker.is_symlink()
    if completion_enabled:
        protected(completion_marker)
        if stat.S_IMODE(completion_marker.lstat().st_mode) != 0o600:
            raise ResetError('Completion owner marker permissions are invalid; nothing was deleted.')
        try:
            marker_text = completion_marker.read_text(encoding='ascii')
        except (OSError, UnicodeError):
            raise ResetError('Completion owner marker is invalid; nothing was deleted.') from None
        marker_value = marker_text[:-1] if marker_text.endswith('\n') else ''
        if not marker_value or len(marker_value) > 10 or not marker_value.isdigit():
            raise ResetError('Completion owner marker is invalid; nothing was deleted.')
        if str(int(marker_value)) != marker_value or int(marker_value) != original_owner:
            raise ResetError('Completion owner marker does not match the service owner; nothing was deleted.')
        fin_executable = root / 'runtime/fin'
        protected(fin_executable)
        if not (fin_executable.lstat().st_mode & stat.S_IXUSR):
            raise ResetError('Installed Fin executable is not executable; nothing was deleted.')

    def command(*args):
        return run(list(args), stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=15,
                   env={'PATH': '/usr/bin:/bin:/usr/sbin:/sbin'})

    fd = os.open(root / 'private/maintenance.lock', os.O_RDONLY | os.O_NOFOLLOW)
    try:
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            raise ResetError('Fin is busy. Finish the current command or approval, then retry.') from None
        if command('/usr/bin/pgrep', '-x', 'fin|fin-touch-id|Fin').returncode != 1:
            raise ResetError('A Fin command or approval is active; finish it before resetting.')
        state = command('/bin/launchctl', 'print', 'system/io.fin-cli.broker').returncode
        if state == 0:
            if command('/bin/launchctl', 'bootout', 'system/io.fin-cli.broker').returncode != 0:
                raise ResetError('Could not stop the Fin service; nothing was deleted.')
        elif state not in (3, 113):
            raise ResetError('Could not inspect the Fin service; nothing was deleted.')
        for _ in range(30):
            status = command('/usr/bin/pgrep', '-f', '^' + str(root / 'runtime/fin-broker') + r'([[:space:]]|$)').returncode
            if status == 1:
                break
            if status != 0:
                raise ResetError('Could not confirm the broker stopped; installation retained.')
            time.sleep(0.1)
        else:
            raise ResetError('The broker has not stopped; installation retained.')
        try:
            empty_journal(root / 'private/operations.sqlite')
        except (ResetError, OSError):
            if state == 0 and command('/bin/launchctl', 'bootstrap', 'system', str(plist)).returncode != 0:
                raise ResetError('Journal verification failed and the service could not restart. No files were deleted; inspect the installation before retrying.') from None
            raise
        # This uses the UID installed in the already root-owned launchd plist,
        # not SUDO_UID (which identifies the person invoking this reset).
        completion_status = 'absent'
        if completion_enabled:
            completion_args = [
                '/usr/bin/sudo', '-n', '-H', '-u', '#' + str(original_owner),
                str(root / 'runtime/fin'), 'internal-completions', 'remove', '--json']
            try:
                completion = run(completion_args, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                                 timeout=15, env={'PATH': '/usr/bin:/bin:/usr/sbin:/sbin'})
            except (OSError, subprocess.SubprocessError):
                if state == 0 and command('/bin/launchctl', 'bootstrap', 'system', str(plist)).returncode != 0:
                    raise ResetError('Shell completion cleanup failed and the service could not restart. Nothing was deleted; inspect the installation before retrying.') from None
                raise ResetError('Could not remove shell completions; installation retained.') from None
            completion_status = None
            try:
                if completion.returncode == 0:
                    payload = json.loads(completion.stdout)
                    candidate = payload.get('status') if isinstance(payload, dict) else None
                    if candidate in ('removed', 'partial', 'absent', 'unsupported'):
                        warnings = payload.get('warnings', [])
                        if isinstance(warnings, list) and all(isinstance(warning, str) for warning in warnings):
                            completion_status = candidate
            except (TypeError, ValueError):
                pass
            if completion_status is None:
                if state == 0 and command('/bin/launchctl', 'bootstrap', 'system', str(plist)).returncode != 0:
                    raise ResetError('Shell completion cleanup failed and the service could not restart. Nothing was deleted; inspect the installation before retrying.')
                raise ResetError('Could not remove shell completions; installation retained.')
        # The exclusive maintenance lock prevents financial/configuration writes
        # across journal inspection and daemon shutdown. No key files are opened.
        registration.unlink()
        plist.unlink()
        shutil.rmtree(root)
        if any(path.exists() or path.is_symlink() for path in (root, plist, registration)):
            raise ResetError('Removal is incomplete. Inspect the installation before fresh setup.')
        return completion_status
    finally:
        os.close(fd)


def main():
    if len(sys.argv) != 1 or sys.platform != 'darwin' or os.geteuid() != 0:
        raise ResetError('Run this reviewed reset with sudo from your own macOS Terminal.')
    if not os.environ.get('SUDO_UID', '').isdigit() or int(os.environ['SUDO_UID']) < 501 or not all(os.isatty(fd) for fd in (0, 1, 2)):
        raise ResetError('Run through sudo in your own Terminal without redirection.')
    completion_status = uninstall()
    if completion_status == 'partial':
        print('Fin uninstalled. Some modified shell completion files were preserved.')
    else:
        print('Fin uninstalled. Saved credentials and configuration deleted; no credential backup created.')


if __name__ == '__main__':
    try:
        main()
    except (ResetError, OSError, subprocess.SubprocessError) as error:
        # Never display arbitrary OS/database output or protected file contents.
        print(str(error) if isinstance(error, ResetError) else 'Reset failed. Inspect installation state before retrying.', file=sys.stderr)
        sys.exit(1)
