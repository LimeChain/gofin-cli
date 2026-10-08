"""Small ownership primitives shared by local synthetic runners.

This module is development tooling only. It does not read Fin configuration or
credentials. Process markers are hints until PID, start identity and argv all
match; a stale marker is never authority to signal a reused PID.
"""
from __future__ import annotations

from dataclasses import dataclass
import fcntl
import hashlib
import json
import os
from pathlib import Path
import signal
import socket
import stat
import subprocess
import sys
import time
from typing import Mapping, Sequence


@dataclass
class LoopbackLease:
    """A reserved ephemeral loopback port. Close immediately before binding."""

    socket: socket.socket

    @property
    def port(self) -> int:
        return int(self.socket.getsockname()[1])

    @property
    def url(self) -> str:
        return f"http://127.0.0.1:{self.port}"

    def release(self) -> None:
        self.socket.close()


def allocate_loopback(attempts: int = 8) -> LoopbackLease:
    """Reserve an independently allocated 127.0.0.1 TCP endpoint."""
    if not 1 <= attempts <= 32:
        raise ValueError("attempts must be between 1 and 32")
    last: OSError | None = None
    for _ in range(attempts):
        candidate = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        candidate.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 0)
        try:
            candidate.bind(("127.0.0.1", 0))
            candidate.listen(1)
            return LoopbackLease(candidate)
        except OSError as error:
            candidate.close()
            last = error
    raise OSError(f"could not reserve a loopback port after {attempts} attempts") from last


def _process_start_identity(pid: int) -> str | None:
    if os.name == "posix" and Path(f"/proc/{pid}/stat").exists():
        try:
            # comm can contain spaces and parentheses; fields after its final ')'
            # start at state (field 3), so starttime is item 22 / offset 19.
            stat = Path(f"/proc/{pid}/stat").read_text()
            fields = stat[stat.rfind(")") + 2 :].split()
            return f"linux:{fields[19]}"
        except (OSError, IndexError):
            return None
    try:
        result = subprocess.run(
            ["ps", "-o", "lstart=", "-p", str(pid)],
            check=True, capture_output=True, text=True, timeout=2,
        )
        value = result.stdout.strip()
        return f"ps:{value}" if value else None
    except (OSError, subprocess.SubprocessError):
        return None


def _process_command(pid: int) -> str | None:
    proc_cmdline = Path(f"/proc/{pid}/cmdline")
    if proc_cmdline.exists():
        try:
            return proc_cmdline.read_bytes().rstrip(b"\0").decode(errors="replace")
        except OSError:
            return None
    try:
        result = subprocess.run(
            ["ps", "-o", "command=", "-p", str(pid)],
            check=True, capture_output=True, text=True, timeout=2,
        )
        value = result.stdout.strip()
        return value or None
    except (OSError, subprocess.SubprocessError):
        return None


def _argv_digest(argv: Sequence[str] | str) -> str:
    raw = argv if isinstance(argv, str) else json.dumps(list(argv), separators=(",", ":"))
    return hashlib.sha256(raw.encode()).hexdigest()


def process_marker(pid: int, argv: Sequence[str] | None = None) -> dict[str, object]:
    current = _process_command(pid) if argv is None else list(argv)
    started = _process_start_identity(pid)
    if not current or not started:
        raise RuntimeError(f"could not verify process identity for pid {pid}")
    return {"pid": pid, "started": started, "argv_sha256": _argv_digest(current)}


def _stable_process_marker(pid: int, timeout: float = 1.0) -> dict[str, object]:
    """Wait out shebang/interpreter exec transitions before recording argv."""
    deadline = time.monotonic() + timeout
    previous: dict[str, object] | None = None
    while time.monotonic() < deadline:
        current = process_marker(pid)
        if previous and current == previous:
            return current
        previous = current
        time.sleep(0.03)
    if previous is None:
        raise RuntimeError(f"could not observe process identity for pid {pid}")
    return previous


def process_matches(marker: Mapping[str, object]) -> bool:
    """Return true only while the PID still has the recorded start and argv."""
    try:
        pid = int(marker["pid"])
        if pid <= 1 or _process_start_identity(pid) != marker["started"]:
            return False
        command = _process_command(pid)
        return bool(command) and _argv_digest(command) == marker["argv_sha256"]
    except (KeyError, TypeError, ValueError, OSError):
        return False


def process_group_alive(marker: Mapping[str, object]) -> bool:
    """Check whether any process remains in the exact recorded session group."""
    try:
        pgid = int(marker["pgid"])
        if pgid <= 1 or pgid != int(marker["pid"]):
            return False
        if Path("/proc").is_dir():
            for process in Path("/proc").iterdir():
                if not process.name.isdigit():
                    continue
                try:
                    fields = (process / "stat").read_text()[
                        (process / "stat").read_text().rfind(")") + 2 :
                    ].split()
                    if int(fields[2]) == pgid and fields[0] != "Z":
                        return True
                except (OSError, ValueError, IndexError):
                    continue
            return False
        result = subprocess.run(
            ["ps", "-axo", "pid=,pgid=,stat="],
            capture_output=True, text=True, timeout=2,
        )
        if result.returncode != 0:
            os.killpg(pgid, 0)
            return True
        found_group = False
        for line in result.stdout.splitlines():
            parts = line.split(None, 2)
            if len(parts) == 3:
                try:
                    same_group = int(parts[1]) == pgid
                except ValueError:
                    continue
                if same_group:
                    found_group = True
                    if not parts[2].startswith("Z"):
                        return True
        if found_group:
            return False
        try:
            os.killpg(pgid, 0)
        except ProcessLookupError:
            return False
        return True
    except ProcessLookupError:
        return False
    except (KeyError, TypeError, ValueError, PermissionError, OSError):
        # Unknown ownership state must keep the marker and block cleanup.
        return True


def write_marker(path: Path, marker: Mapping[str, object]) -> None:
    """Atomically replace a private JSON process marker."""
    path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    temporary = path.with_name(path.name + f".{os.getpid()}.tmp")
    fd = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as output:
            json.dump(dict(marker), output, sort_keys=True)
            output.write("\n")
            output.flush()
            os.fsync(output.fileno())
        os.replace(temporary, path)
    finally:
        try:
            temporary.unlink()
        except FileNotFoundError:
            pass


def read_marker(path: Path) -> dict[str, object] | None:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return None
    return value if isinstance(value, dict) else None


def stop_owned_process(marker: Mapping[str, object], timeout: float = 5.0) -> bool:
    """Stop only a process that still matches its complete recorded identity."""
    if not process_matches(marker):
        return not process_group_alive(marker)
    pid = int(marker["pid"])
    try:
        pgid = int(marker.get("pgid", pid))
        if pgid != pid or os.getpgid(pid) != pgid:
            return False
        os.killpg(pgid, signal.SIGTERM)
    except ProcessLookupError:
        return True
    deadline = time.monotonic() + max(0.0, timeout)
    while time.monotonic() < deadline:
        if not process_group_alive(marker):
            return True
        time.sleep(0.05)
    if process_matches(marker):
        os.killpg(int(marker.get("pgid", pid)), signal.SIGKILL)
    deadline = time.monotonic() + max(0.0, timeout)
    while process_group_alive(marker) and time.monotonic() < deadline:
        time.sleep(0.05)
    return not process_group_alive(marker)


@dataclass
class OwnedProcess:
    process: subprocess.Popen[bytes]
    marker: dict[str, object]
    marker_path: Path
    log_file: object

    def stop(self, timeout: float = 5.0) -> bool:
        stopped = stop_owned_process(self.marker, timeout)
        if not stopped and self.process.poll() is not None and not process_group_alive(self.marker):
            persisted = read_marker(self.marker_path)
            if persisted == self.marker:
                self.marker_path.unlink(missing_ok=True)
                stopped = True
        if stopped:
            try:
                self.process.wait(timeout=max(0.0, timeout))
            except subprocess.TimeoutExpired:
                pass
            try:
                self.marker_path.unlink()
            except FileNotFoundError:
                pass
        self.log_file.close()
        return stopped

    def detach(self) -> None:
        """Close parent log ownership after a supervised startup succeeds."""
        if not self.log_file.closed:
            self.log_file.close()
        # The runner uses the durable identity marker across invocations. Keep
        # Popen.poll()/wait() functional, but do not warn at interpreter exit
        # because this intentionally long-lived child is not being reaped here.
        self.process._child_created = False  # type: ignore[attr-defined]


def start_owned_process(
    command: Sequence[str],
    *,
    log_path: Path,
    marker_path: Path,
    env: Mapping[str, str] | None = None,
    marker_fields: Mapping[str, object] | None = None,
) -> OwnedProcess:
    """Start an owned process group and persist enough identity to stop safely."""
    if not command or not all(isinstance(arg, str) for arg in command):
        raise ValueError("command must be a nonempty sequence of strings")
    launch_command = list(command)
    try:
        if Path(launch_command[0]).read_bytes().startswith(b"#!/usr/bin/env python"):
            launch_command.insert(0, sys.executable)
    except OSError:
        pass
    log_path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    marker_path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    log_file = log_path.open("xb")
    try:
        process = subprocess.Popen(
            launch_command, stdin=subprocess.DEVNULL, stdout=log_file,
            stderr=subprocess.STDOUT, env=None if env is None else dict(env),
            start_new_session=True, close_fds=True,
        )
        marker = _stable_process_marker(process.pid)
        marker["pgid"] = process.pid
        marker.update(dict(marker_fields or {}))
        write_marker(marker_path, marker)
        return OwnedProcess(process, marker, marker_path, log_file)
    except BaseException:
        if "process" in locals():
            try:
                os.killpg(process.pid, signal.SIGTERM)
                process.wait(timeout=2)
            except (OSError, subprocess.SubprocessError):
                try:
                    os.killpg(process.pid, signal.SIGKILL)
                    process.wait(timeout=2)
                except (OSError, subprocess.SubprocessError):
                    pass
        log_file.close()
        raise


class EnvironmentLock:
    """Serialize lifecycle actions for one environment using an owned lockfile."""

    def __init__(self, path: Path):
        self.path = path
        self.fd: int | None = None

    def __enter__(self) -> "EnvironmentLock":
        self.path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
        self.fd = os.open(self.path, os.O_RDWR | os.O_CREAT, 0o600)
        fcntl.flock(self.fd, fcntl.LOCK_EX)
        return self

    def __exit__(self, *_: object) -> None:
        assert self.fd is not None
        fcntl.flock(self.fd, fcntl.LOCK_UN)
        os.close(self.fd)
        self.fd = None


def broker_lock_is_held(path: Path) -> bool:
    """Observe broker lifetime flock without holding it across file mutations."""
    path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    try:
        existing = path.lstat()
        if not stat.S_ISREG(existing.st_mode):
            raise RuntimeError(f"broker lock path is not a regular file: {path}")
    except FileNotFoundError:
        pass
    fd = os.open(path, os.O_RDWR | os.O_CREAT, 0o600)
    try:
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            return True
        else:
            fcntl.flock(fd, fcntl.LOCK_UN)
            return False
    finally:
        os.close(fd)
