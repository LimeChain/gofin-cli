#!/usr/bin/python3
"""Human-only local beta reset. No credential backup, network or financial calls."""
import fcntl
import json
import os
from pathlib import Path
import plistlib
import shutil
import stat
import subprocess
import sys
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


def protect_financial_storage(private):
    # Do not parse current/history data during removal. Keep owner and file-type
    # checks for present SQLite files while allowing missing or damaged stores.
    database = private / 'operations.sqlite'
    for path in (database, database.with_name(database.name + '-wal'),
                 database.with_name(database.name + '-shm')):
        if path.exists() or path.is_symlink():
            protected(path)


def confirm_data_loss(input_stream=sys.stdin, output_stream=sys.stderr):
    output_stream.write(
        'Uninstall permanently deletes saved credentials, settings and financial history, including '
        'pending records used for recovery or avoiding duplicates. It does not cancel existing '
        'transactions or orders on exchanges or chains. No backup is created.\n'
        'Delete GoFin and all saved data? [y/N] '
    )
    output_stream.flush()
    try:
        answer = input_stream.readline()
    except KeyboardInterrupt:
        output_stream.write('\n')
        output_stream.flush()
        return None
    return answer.strip().lower() in ('y', 'yes')


def uninstall(root=ROOT, plist=PLIST, registration=REGISTRATION, run=subprocess.run,
              confirm=confirm_data_loss):
    protected(root, True)
    protected(root / 'private', True)
    protected(root / 'private/maintenance.lock')
    protect_financial_storage(root / 'private')
    protected(plist)
    protected(registration)
    if registration.read_text() != str(root / 'runtime') + '\n':
        raise ResetError('PATH registration differs from this installation; nothing was deleted.')
    try:
        service = plistlib.loads(plist.read_bytes())
    except Exception:
        raise ResetError('Service registration is invalid; nothing was deleted.') from None
    arguments = service.get('ProgramArguments')
    installed_name = next((name for name in ('gofin', 'fin')
                           if arguments == [str(root / ('runtime/' + name + '-broker'))]), None)
    if service.get('Label') != 'io.fin-cli.broker' or installed_name is None:
        raise ResetError('Service registration differs from this installation; nothing was deleted.')
    installed_cli = root / 'runtime' / installed_name
    installed_broker = root / 'runtime' / (installed_name + '-broker')
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
        gofin_executable = installed_cli
        protected(gofin_executable)
        if not (gofin_executable.lstat().st_mode & stat.S_IXUSR):
            raise ResetError('Installed GoFin executable is not executable; nothing was deleted.')

    def command(*args):
        return run(list(args), stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=15,
                   env={'PATH': '/usr/bin:/bin:/usr/sbin:/sbin'})

    fd = os.open(root / 'private/maintenance.lock', os.O_RDONLY | os.O_NOFOLLOW)
    try:
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            raise ResetError('GoFin is busy. Finish the current command or approval, then retry.') from None
        if command('/usr/bin/pgrep', '-x', 'fin|fin-touch-id|Fin|gofin|gofin-touch-id|GoFin').returncode != 1:
            raise ResetError('A GoFin command or approval is active; finish it before resetting.')
        state = command('/bin/launchctl', 'print', 'system/io.fin-cli.broker').returncode
        if state not in (0, 3, 113):
            raise ResetError('Could not inspect the GoFin service; nothing was deleted.')
        # All checks above are read-only. Ask before stopping launchd or changing
        # completion files, credentials, configuration, journal, or install paths.
        confirmation = confirm()
        if confirmation is None:
            return 'interrupted'
        if not confirmation:
            return 'cancelled'
        if state == 0 and command('/bin/launchctl', 'bootout', 'system/io.fin-cli.broker').returncode != 0:
            raise ResetError('Could not stop the GoFin service; nothing was deleted.')
        for _ in range(30):
            status = command('/usr/bin/pgrep', '-f', '^' + str(installed_broker) + r'([[:space:]]|$)').returncode
            if status == 1:
                break
            if status != 0:
                raise ResetError('Could not confirm the broker stopped; installation retained.')
            time.sleep(0.1)
        else:
            raise ResetError('The broker has not stopped; installation retained.')
        # This uses the UID installed in the already root-owned launchd plist,
        # not SUDO_UID (which identifies the person invoking this reset).
        completion_status = 'absent'
        if completion_enabled:
            completion_args = [
                '/usr/bin/sudo', '-n', '-H', '-u', '#' + str(original_owner),
                str(installed_cli), 'internal-completions', 'remove', '--json']
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
        # The exclusive maintenance lock prevents new financial/configuration
        # writes while the service is stopped and GoFin-owned state is removed.
        registration.unlink()
        plist.unlink()
        shutil.rmtree(root)
        if any(path.exists() or path.is_symlink() for path in (root, plist, registration)):
            raise ResetError('Removal is incomplete. Inspect the installation before fresh setup.')
        return completion_status
    finally:
        os.close(fd)


def main(root=ROOT, plist=PLIST, registration=REGISTRATION, run=subprocess.run,
         input_stream=None, prompt_stream=None, result_stream=None):
    sudo_uid = os.environ.get('SUDO_UID', '')
    prompt_stream = sys.stderr if prompt_stream is None else prompt_stream
    result_stream = sys.stdout if result_stream is None else result_stream
    input_stream = sys.stdin if input_stream is None else input_stream
    if len(sys.argv) != 1 or sys.platform != 'darwin' or os.geteuid() != 0:
        raise ResetError('Run this reviewed reset with sudo from your own macOS Terminal.')
    if not sudo_uid.isdigit() or int(sudo_uid) < 501 or not all(os.isatty(fd) for fd in (0, 1, 2)):
        raise ResetError('Run through sudo in your own Terminal without redirection.')
    completion_status = uninstall(
        root, plist, registration, run,
        confirm=lambda: confirm_data_loss(input_stream, prompt_stream),
    )
    if completion_status == 'interrupted':
        print('Uninstall cancelled; GoFin and its saved data were not changed.', file=prompt_stream)
        return 130
    if completion_status == 'cancelled':
        print('Uninstall cancelled; GoFin and its saved data were not changed.', file=result_stream)
        return 1
    if completion_status == 'partial':
        print('GoFin uninstalled. Saved credentials, settings and financial history deleted. Some modified completion files were preserved.', file=result_stream)
    else:
        print('GoFin uninstalled. Saved credentials, settings and financial history deleted.', file=result_stream)
    return 0


if __name__ == '__main__':
    try:
        sys.exit(main())
    except (ResetError, OSError, subprocess.SubprocessError) as error:
        # Never display arbitrary OS/database output or protected file contents.
        print(str(error) if isinstance(error, ResetError) else 'Reset failed. Inspect installation state before retrying.', file=sys.stderr)
        sys.exit(1)
