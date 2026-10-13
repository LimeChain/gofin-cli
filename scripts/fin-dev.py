#!/usr/bin/env python3
"""Run isolated synthetic or disposable live Fin broker environments."""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import signal
import socket
import stat
import getpass
import subprocess
import sys
import threading
import time

from fin_dev_resources import (
    EnvironmentLock, broker_lock_is_held, process_group_alive, process_matches, read_marker,
    start_owned_process, stop_owned_process,
)


REPO = Path(__file__).resolve().parent.parent
BROKER_REL = Path("target/debug/examples/fin-dev-broker")
CLI_REL = Path("target/debug/examples/fin-dev")
START_TIMEOUT = 15.0
VALID_MODES = {"synthetic", "live"}
VALID_FIXTURES = {"solana"}


class DevError(Exception):
    def __init__(self, code: str, message: str, next_step: str):
        super().__init__(message)
        self.code, self.next_step = code, next_step


def worktree_key() -> str:
    return hashlib.sha256(str(REPO.resolve()).encode()).hexdigest()[:20]


def state_root() -> Path:
    override = os.environ.get("FIN_DEV_STATE_HOME")
    if override:
        return Path(override).expanduser().resolve()
    base = Path(os.environ.get("XDG_STATE_HOME", Path.home() / ".local/state"))
    return base / "fin" / "dev"


def validate_name(value: str) -> str:
    if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_.-]{0,47}", value) or value in {".", ".."}:
        raise DevError("ENVIRONMENT_NAME_INVALID", "Use a name with letters, numbers, dot, dash or underscore.", "Retry with a short environment name, such as feature-a.")
    return value


def env_dir(name: str) -> Path:
    return state_root() / worktree_key() / validate_name(name)


def runtime_socket_path(name: str) -> Path:
    # macOS limits AF_UNIX paths to roughly 104 bytes. Keep this deterministic,
    # short, private and distinct for each worktree/name pair.
    owner = os.getuid() if hasattr(os, "getuid") else getpass.getuser()
    token = hashlib.sha256((str(REPO.resolve()) + "\0" + str(state_root()) + "\0" + validate_name(name)).encode()).hexdigest()[:18]
    return Path("/tmp") / f"fin-dev-{owner}" / f"{token}.sock"


def lock_path(name: str) -> Path:
    return state_root() / ".locks" / f"{worktree_key()}-{validate_name(name)}.lock"


def build_lock_path() -> Path:
    return state_root() / ".locks" / f"{worktree_key()}-build.lock"


def identity_for(broker: Path, cli: Path) -> str:
    digest = hashlib.sha256()
    for label, path in ((b"fin-dev", cli), (b"fin-dev-broker", broker)):
        try:
            contents = path.read_bytes()
        except OSError as error:
            raise DevError("DEV_BUILD_MISSING", "The development binaries are not built.", "Run `python3 scripts/fin-dev.py start NAME` to build this worktree.") from error
        digest.update(label + b"\0" + hashlib.sha256(contents).digest())
    return digest.hexdigest()


def source_build_id() -> str:
    """Stable pre-build identity embedded in both development examples."""
    digest = hashlib.sha256()
    candidates = [REPO / name for name in ("Cargo.toml", "Cargo.lock", "rust-toolchain.toml", "rustfmt.toml", "skills/fin/SKILL.md")]
    for base in (REPO / "crates",):
        for root, dirs, files in os.walk(base):
            dirs[:] = sorted(d for d in dirs if d not in {"target", ".git"})
            for filename in files:
                path = Path(root) / filename
                if filename == "Cargo.toml" or filename.endswith((".rs", ".json")) or filename == "build.rs":
                    candidates.append(path)
    for path in sorted(set(candidates)):
        if path.is_file():
            relative = path.relative_to(REPO).as_posix().encode()
            digest.update(len(relative).to_bytes(4, "big") + relative)
            digest.update(hashlib.sha256(path.read_bytes()).digest())
    return digest.hexdigest()


def artifacts() -> tuple[Path, Path, str]:
    broker, cli = REPO / BROKER_REL, REPO / CLI_REL
    return broker, cli, identity_for(broker, cli)


def read_environment(path: Path) -> dict | None:
    try:
        result = json.loads((path / "environment.json").read_text())
    except (OSError, json.JSONDecodeError):
        return None
    return result if isinstance(result, dict) else None


def owned_marker(path: Path) -> dict | None:
    marker = read_marker(path / "broker.process.json")
    if marker is None or not process_matches(marker):
        return None
    return marker


def valid_metadata(path: Path, name: str, metadata: dict | None) -> bool:
    if not metadata:
        return False
    expected = {
        "worktree": str(REPO.resolve()),
        "worktree_key": worktree_key(),
        "name": name,
        "socket": str(runtime_socket_path(name)),
        "config": str(path / "config.json"),
        "database": str(path / "operations.sqlite"),
        "runtime_cli": str(path / "runtime/fin-dev"),
        "runtime_broker": str(path / "runtime/fin-dev-broker"),
        "environment_id": hashlib.sha256(f"{worktree_key()}:{name}".encode()).hexdigest(),
    }
    return (
        metadata.get("mode", "synthetic") in VALID_MODES
        and all(metadata.get(key) == value for key, value in expected.items())
    )


def same_owned_socket(path: Path, name: str, metadata: dict | None) -> bool:
    if not metadata:
        return False
    socket_path = runtime_socket_path(name)
    if metadata.get("socket") != str(socket_path):
        return False
    try:
        st = socket_path.lstat()
    except FileNotFoundError:
        return False
    return (st.st_dev, st.st_ino) == (metadata.get("socket_dev"), metadata.get("socket_ino"))


def socket_is_active(path: Path) -> bool:
    try:
        if not stat.S_ISSOCK(path.lstat().st_mode):
            return False
    except FileNotFoundError:
        return False
    peer = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    peer.settimeout(0.25)
    try:
        peer.connect(str(path))
        return True
    except (ConnectionRefusedError, FileNotFoundError):
        return False
    except OSError:
        # An inconclusive live socket must be treated as occupied.
        return True
    finally:
        peer.close()


def atomic_json(path: Path, obj: dict) -> None:
    temporary = path.with_name(path.name + f".{os.getpid()}.tmp")
    temporary.write_text(json.dumps(obj, sort_keys=True, indent=2) + "\n")
    os.chmod(temporary, 0o600)
    os.replace(temporary, path)


def wait_ready(path: Path, process: subprocess.Popen, timeout: float, socket_path: Path,
               identity: str, build_id: str, mode: str, log_path: Path) -> tuple[int, int]:
    deadline = time.monotonic() + timeout
    sock = socket_path
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise DevError("BROKER_START_FAILED", "The development broker exited during startup.", f"Inspect {log_path} and retry start.")
        try:
            st = sock.lstat()
            if not stat.S_ISSOCK(st.st_mode):
                raise DevError("BROKER_SOCKET_OCCUPIED", "The selected broker socket path is occupied by an unexpected file.", "Inspect the selected environment path; reset only after verifying its contents.")
            client = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            client.settimeout(0.15)
            try:
                client.connect(str(sock))
                hello = json.dumps({"kind": "hello", "identity": identity,
                                    "build_id": build_id, "mode": mode}, separators=(",", ":")).encode() + b"\n"
                client.sendall(hello)
                response = b""
                while not response.endswith(b"\n") and len(response) <= 8192:
                    chunk = client.recv(4096)
                    if not chunk:
                        break
                    response += chunk
                answer = json.loads(response)
                if answer != {"kind": "hello", "identity": identity, "build_id": build_id,
                              "mode": mode}:
                    raise DevError("BROKER_IDENTITY_MISMATCH", "The socket peer did not prove the selected environment and build identity.", "Inspect the selected socket owner; startup did not accept this broker.")
                return st.st_dev, st.st_ino
            finally:
                client.close()
        except DevError:
            raise
        except (OSError, FileNotFoundError):
            pass
        time.sleep(0.05)
    raise DevError("BROKER_START_TIMEOUT", "The development broker did not become ready in time.", f"Inspect {log_path} and retry start.")


def ensure_stopped_or_clear_stale(path: Path) -> dict | None:
    metadata = read_environment(path)
    marker_path = path / "broker.process.json"
    marker = read_marker(marker_path)
    name = path.name
    if metadata and not valid_metadata(path, name, metadata):
        raise DevError("ENVIRONMENT_METADATA_MISMATCH", "The environment metadata does not belong to this worktree and name.", "Inspect the selected directory; no process or socket was touched.")
    if broker_lock_is_held(path / "broker.lock"):
        raise DevError("ENVIRONMENT_ALREADY_RUNNING", "The broker still holds this environment's lifetime lock.", "Stop the selected environment before starting it again.")
    if marker and process_group_alive(marker):
        if (not metadata or marker.get("identity") != metadata.get("environment_identity")
                or marker.get("build_id") != metadata.get("build_id")
                or marker.get("environment_id") != metadata.get("environment_id")
                or marker.get("mode") != metadata.get("mode", "synthetic")):
            raise DevError("PROCESS_OWNERSHIP_MISMATCH", "The live process marker does not match this environment identity.", "Inspect the selected process; no signal was sent.")
        if not process_matches(marker):
            raise DevError("PROCESS_OWNERSHIP_MISMATCH", "The broker leader exited while its owned process group remains.", "Inspect the process group; start/reset left it untouched.")
        raise DevError("ENVIRONMENT_ALREADY_RUNNING", "This environment already has its owned broker running.", "Use `cli NAME ...` or stop it before restarting.")
    # An occupied socket without matching inode evidence could belong to another
    # process. Keep it and fail closed; never unlink an unknown listener/path.
    socket_path = runtime_socket_path(name) if metadata else path / "broker.sock"
    if socket_path.exists() or socket_path.is_symlink():
        if socket_is_active(socket_path):
            raise DevError("ENVIRONMENT_ALREADY_RUNNING", "The selected socket still accepts connections, even though its process marker is missing or stale.", "Inspect the broker owner; no state was removed.")
        if not same_owned_socket(path, name, metadata):
            raise DevError("BROKER_SOCKET_OCCUPIED", "A stale or unexpected socket occupies this environment path.", "Inspect the socket and remove it only after confirming its owner.")
        socket_path.unlink()
    if marker_path.exists():
        marker_path.unlink()
    return metadata


def build_snapshot(directory: Path) -> tuple[str, Path, Path, str]:
    """Build once under a worktree lock, then copy an immutable binary pair."""
    with EnvironmentLock(build_lock_path()):
        build_id = source_build_id()
        build_env = dict(os.environ, FIN_DEV_BUILD_ID=build_id)
        proc = subprocess.run(
            ["cargo", "build", "--offline", "--locked", "--target-dir", str(REPO / "target"), "-p", "fin-cli", "-p", "fin-broker", "--examples"],
            cwd=REPO, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
            stderr=subprocess.PIPE, text=True, env=build_env,
        )
        if proc.returncode:
            logs = directory / "logs"
            logs.mkdir(parents=True, exist_ok=True, mode=0o700)
            build_log = logs / f"build-{time.time_ns()}.log"
            build_log.write_text(proc.stdout + proc.stderr)
            os.chmod(build_log, 0o600)
            raise DevError("DEV_BUILD_FAILED", "The development binaries could not be built.", f"Inspect {build_log}; saved environment state was preserved.")
        if source_build_id() != build_id:
            raise DevError("SOURCE_CHANGED_DURING_BUILD", "Worktree source changed while the development binaries were building.", "Retry start after the edit is complete; saved environment state was preserved.")
        broker_bin, cli_bin, _ = artifacts()
        if not broker_bin.is_file() or not cli_bin.is_file():
            raise DevError("DEV_BUILD_MISSING", "The expected development CLI or broker example was not produced.", "Check the example target names and rerun start.")
        runtime = directory / "runtime"
        runtime.mkdir(mode=0o700, exist_ok=True)
        runtime_cli, runtime_broker = runtime / "fin-dev", runtime / "fin-dev-broker"
        shutil.copyfile(cli_bin, runtime_cli)
        shutil.copyfile(broker_bin, runtime_broker)
        os.chmod(runtime_cli, 0o700)
        os.chmod(runtime_broker, 0o700)
        return build_id, runtime_cli, runtime_broker, identity_for(runtime_broker, runtime_cli)


def start(name: str, machine: bool, mode: str = "synthetic", fixture: str | None = None) -> dict:
    name = validate_name(name)
    if mode not in VALID_MODES:
        raise DevError("DEV_MODE_INVALID", "Choose synthetic or live development mode.", "Retry with `--mode synthetic` or `--mode live`.")
    if fixture not in {None, *VALID_FIXTURES} or (fixture is not None and mode != "synthetic"):
        raise DevError("DEV_FIXTURE_INVALID", "Development fixtures are available only in synthetic mode.", "Choose `--fixture solana` with `--mode synthetic`, or omit the fixture.")
    directory = env_dir(name)
    with EnvironmentLock(lock_path(name)):
        directory.mkdir(parents=True, exist_ok=True, mode=0o700)
        os.chmod(directory, 0o700)
        previous = ensure_stopped_or_clear_stale(directory)
        saved_mode = previous.get("mode", "synthetic") if previous else None
        saved_fixture = previous.get("fixture") if previous else None
        if saved_mode and saved_mode != mode:
            raise DevError("DEV_MODE_MISMATCH", f"Environment {name} is saved in {saved_mode} mode.", "Use its saved mode when starting. Reset a stopped synthetic environment before switching modes; live environments keep their financial history.")
        if previous and saved_fixture != fixture:
            raise DevError("DEV_FIXTURE_MISMATCH", f"Environment {name} was created with a different fixture selection.", "Use the same fixture or reset the stopped synthetic environment before changing fixtures.")
        (directory / "logs").mkdir(mode=0o700, exist_ok=True)
        build_id, runtime_cli, runtime_broker, binary_id = build_snapshot(directory)
        runtime_socket = runtime_socket_path(name)
        runtime_socket.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
        os.chmod(runtime_socket.parent, 0o700)
        environment_id = hashlib.sha256(f"{worktree_key()}:{name}".encode()).hexdigest()
        environment_identity = hashlib.sha256(
            f"{environment_id}:{build_id}:{time.time_ns()}:{os.getpid()}".encode()
        ).hexdigest()
        identity = {
            "format": 1,
            "name": name,
            "worktree": str(REPO.resolve()),
            "worktree_key": worktree_key(),
            "build_id": build_id,
            "binary_id": binary_id,
            "environment_identity": environment_identity,
            "environment_id": environment_id,
            "mode": mode,
            "fixture": fixture,
            "cli_sha256": hashlib.sha256(runtime_cli.read_bytes()).hexdigest(),
            "broker_sha256": hashlib.sha256(runtime_broker.read_bytes()).hexdigest(),
            "socket": str(runtime_socket),
            "config": str(directory / "config.json"),
            "database": str(directory / "operations.sqlite"),
            "runtime_cli": str(runtime_cli),
            "runtime_broker": str(runtime_broker),
        }
        atomic_json(directory / "environment.json", identity)
        env = dict(os.environ)
        env.update({
            "FIN_DEV_SOCKET": identity["socket"],
            "FIN_DEV_CONFIG": identity["config"],
            "FIN_DEV_DATABASE": identity["database"],
            "FIN_DEV_IDENTITY": environment_identity,
            "FIN_DEV_BUILD_ID": build_id,
            "FIN_DEV_ENVIRONMENT_ID": environment_id,
            "FIN_DEV_MODE": mode,
        })
        # Runtime accepts these paths only in this development-only executable.
        command = [str(runtime_broker), "--socket", identity["socket"], "--database", identity["database"],
                   "--config", identity["config"], "--identity", environment_identity,
                   "--build-id", build_id, "--environment-id", environment_id,
                   "--environment-name", name, "--mode", mode]
        if fixture:
            command.extend(("--fixture", fixture))
        try:
            log_path = directory / "logs" / f"broker-{time.time_ns()}.log"
            owned = start_owned_process(command, log_path=log_path,
                                        marker_path=directory / "broker.process.json", env=env,
                                        marker_fields={"identity": environment_identity, "build_id": build_id,
                                                       "environment_id": environment_id, "mode": mode})
            try:
                dev, ino = wait_ready(directory, owned.process, START_TIMEOUT, runtime_socket,
                                      environment_identity, build_id, mode, log_path)
            except BaseException:
                owned.stop()
                raise
            identity["socket_dev"], identity["socket_ino"] = dev, ino
            atomic_json(directory / "environment.json", identity)
            owned.detach()
        except KeyboardInterrupt:
            raise DevError("START_CANCELLED", "Environment startup was cancelled; any broker started by this command was stopped.", "Retry `start NAME` when ready.")
        return {"ok": True, "status": "running", "environment": name, "buildId": build_id, "mode": mode}


def credentials_file(value: str) -> Path:
    """Validate the selected file without reading or parsing its contents."""
    try:
        path = Path(value).expanduser().resolve(strict=True)
        metadata = path.stat()
    except (OSError, RuntimeError):
        raise DevError("CREDENTIAL_FILE_UNAVAILABLE", "The selected credentials file is unavailable.", "Check the file in your own terminal and retry the import.") from None
    if not stat.S_ISREG(metadata.st_mode) or not os.access(path, os.R_OK):
        raise DevError("CREDENTIAL_FILE_UNAVAILABLE", "The selected credentials file is unavailable.", "Choose a readable regular file and retry the import.")
    return path


def stop_import_process(process: subprocess.Popen[bytes], timeout: float = 5.0) -> None:
    """Stop the foreground importer session created by this invocation only."""
    if process.poll() is not None:
        return
    try:
        # start_new_session=True makes the child PID its unique process-group
        # leader. The offline Rust importer has no child processes.
        os.killpg(process.pid, signal.SIGTERM)
    except ProcessLookupError:
        return
    deadline = time.monotonic() + timeout
    while process.poll() is None and time.monotonic() < deadline:
        time.sleep(0.05)
    if process.poll() is None:
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        try:
            process.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            pass


def import_credentials(name: str, file_value: str, discover_identities: bool = False,
                       provider: str | None = None) -> dict:
    """Ask the development-only Rust broker to import a user-selected file."""
    name = validate_name(name)
    if provider not in {None, "binance", "evm", "solana"}:
        raise DevError("CREDENTIAL_PROVIDER_INVALID", "The selected credential provider is unsupported.", "Choose binance, evm or solana.")
    if discover_identities and provider not in {None, "binance"}:
        raise DevError("CREDENTIAL_PROVIDER_INVALID", "Identity discovery is available only for Binance imports.", "Retry without identity discovery or select the Binance provider.")
    selected_file = credentials_file(file_value)
    directory = env_dir(name)
    with EnvironmentLock(lock_path(name)):
        directory.mkdir(parents=True, exist_ok=True, mode=0o700)
        os.chmod(directory, 0o700)
        previous = ensure_stopped_or_clear_stale(directory)
        saved_mode = previous.get("mode", "synthetic") if previous else None
        if saved_mode and saved_mode != "live":
            raise DevError("DEV_MODE_MISMATCH", f"Environment {name} is saved in {saved_mode} mode.", "Explicitly reset the stopped synthetic environment before importing live credentials.")
        if broker_lock_is_held(directory / "broker.lock"):
            raise DevError("ENVIRONMENT_ALREADY_RUNNING", "The environment broker still owns its persistent state.", "Stop the selected environment before importing credentials.")

        (directory / "logs").mkdir(mode=0o700, exist_ok=True)
        build_id, runtime_cli, runtime_broker, binary_id = build_snapshot(directory)
        environment_id = hashlib.sha256(f"{worktree_key()}:{name}".encode()).hexdigest()
        environment_identity = hashlib.sha256(
            f"{environment_id}:{build_id}:{time.time_ns()}:{os.getpid()}:import".encode()
        ).hexdigest()
        identity = dict(previous or {})
        identity.update({
            "format": 1,
            "name": name,
            "worktree": str(REPO.resolve()),
            "worktree_key": worktree_key(),
            "build_id": build_id,
            "binary_id": binary_id,
            "environment_identity": environment_identity,
            "environment_id": environment_id,
            "mode": "live",
            "cli_sha256": hashlib.sha256(runtime_cli.read_bytes()).hexdigest(),
            "broker_sha256": hashlib.sha256(runtime_broker.read_bytes()).hexdigest(),
            "socket": str(runtime_socket_path(name)),
            "config": str(directory / "config.json"),
            "database": str(directory / "operations.sqlite"),
            "runtime_cli": str(runtime_cli),
            "runtime_broker": str(runtime_broker),
        })
        environment = dict(os.environ)
        environment.update({
            "FIN_DEV_SOCKET": identity["socket"],
            "FIN_DEV_CONFIG": identity["config"],
            "FIN_DEV_DATABASE": identity["database"],
            "FIN_DEV_IDENTITY": environment_identity,
            "FIN_DEV_BUILD_ID": build_id,
            "FIN_DEV_ENVIRONMENT_ID": environment_id,
            "FIN_DEV_MODE": "live",
        })
        command = [
            str(runtime_broker), "--socket", identity["socket"],
            "--database", identity["database"], "--config", identity["config"],
            "--identity", environment_identity, "--build-id", build_id,
            "--environment-id", environment_id, "--mode", "live",
            "--environment-name", name,
            "--import-credentials", str(selected_file),
        ]
        if provider:
            command.extend(("--provider", provider))
        if discover_identities:
            command.append("--discover-identities")
        log_path = directory / "logs" / f"credential-import-{time.time_ns()}.log"
        try:
            # Imports are intentionally foreground and often finish before a
            # durable long-running process marker could be observed. Give the
            # short-lived Rust process its own session and private log instead.
            log = log_path.open("xb")
            process = subprocess.Popen(
                command, stdin=subprocess.DEVNULL, stdout=log,
                stderr=subprocess.STDOUT, env=environment,
                start_new_session=True, close_fds=True,
            )
        except OSError:
            if "log" in locals() and not log.closed:
                log.close()
            raise DevError("CREDENTIAL_IMPORT_FAILED", "Credentials could not be imported.", "Check the selected file and Fin development logs, then retry.") from None
        try:
            try:
                status = process.wait(timeout=120 if discover_identities else 60)
            except subprocess.TimeoutExpired:
                stop_import_process(process)
                raise DevError("CREDENTIAL_IMPORT_TIMEOUT", "Credential import did not finish in time.", "Check the selected file and retry the import.") from None
            if status != 0:
                raise DevError("CREDENTIAL_IMPORT_FAILED", "Credentials could not be imported.", "Check that the selected file is a private, readable .env file with supported Fin development keys, then retry.")
        except KeyboardInterrupt:
            stop_import_process(process)
            raise DevError("CREDENTIAL_IMPORT_CANCELLED", "Credential import was cancelled.", "Retry the import when ready; imported credential values are never displayed.") from None
        finally:
            if process.poll() is None:
                stop_import_process(process)
            log.close()
        atomic_json(directory / "environment.json", identity)
        return {"ok": True, "status": "imported", "environment": name, "mode": "live"}


def cli_invocation(name: str, args: list[str]) -> tuple[list[str], dict[str, str]]:
    name = validate_name(name)
    directory = env_dir(name)
    metadata = read_environment(directory)
    marker = owned_marker(directory)
    if not metadata or not valid_metadata(directory, name, metadata):
        raise DevError("ENVIRONMENT_ABSENT", "This worktree has no matching development environment.", "Run `python3 scripts/fin-dev.py start NAME` first.")
    if not marker:
        raise DevError("BROKER_UNAVAILABLE", "The selected environment broker is not running.", "Start this environment before invoking its CLI.")
    cli_bin = Path(metadata.get("runtime_cli", directory / "runtime/fin-dev"))
    broker_bin = Path(metadata.get("runtime_broker", directory / "runtime/fin-dev-broker"))
    if identity_for(broker_bin, cli_bin) != metadata.get("binary_id"):
        raise DevError("BUILD_IDENTITY_MISMATCH", "The selected environment build files changed after startup.", "Stop and restart the environment to rebuild and verify both binaries together.")
    if (metadata.get("environment_identity") != marker.get("identity")
            or metadata.get("build_id") != marker.get("build_id")
            or metadata.get("environment_id") != marker.get("environment_id")
            or metadata.get("mode", "synthetic") != marker.get("mode")):
        raise DevError("BROKER_IDENTITY_MISMATCH", "The selected broker process does not match this environment build.", "Stop this environment and start it again from this worktree.")
    env = dict(os.environ)
    env.update({"FIN_DEV_SOCKET": metadata["socket"], "FIN_DEV_CONFIG": metadata["config"],
                "FIN_DEV_DATABASE": metadata["database"], "FIN_DEV_IDENTITY": metadata["environment_identity"],
                "FIN_DEV_BUILD_ID": metadata["build_id"],
                "FIN_DEV_ENVIRONMENT_ID": metadata["environment_id"],
                "FIN_DEV_MODE": metadata.get("mode", "synthetic")})
    command = [str(cli_bin), "--identity", metadata["environment_identity"],
               "--build-id", metadata["build_id"], "--mode", metadata.get("mode", "synthetic"), *args]
    return command, env


def cli(name: str, args: list[str]) -> int:
    command, env = cli_invocation(name, args)
    return subprocess.run(command, cwd=REPO, env=env).returncode


def stop(name: str) -> dict:
    name = validate_name(name)
    directory = env_dir(name)
    with EnvironmentLock(lock_path(name)):
        metadata = read_environment(directory)
        marker = read_marker(directory / "broker.process.json")
        if metadata and not valid_metadata(directory, name, metadata):
            raise DevError("ENVIRONMENT_METADATA_MISMATCH", "The environment metadata does not belong to this worktree and name.", "Inspect the selected directory; no process or socket was touched.")
        if marker and process_group_alive(marker):
            if (not metadata or marker.get("identity") != metadata.get("environment_identity")
                    or marker.get("build_id") != metadata.get("build_id")
                    or marker.get("environment_id") != metadata.get("environment_id")
                    or marker.get("mode") != metadata.get("mode", "synthetic")):
                raise DevError("PROCESS_OWNERSHIP_MISMATCH", "The live process marker does not match this environment identity.", "Inspect the selected process; no signal was sent.")
            if not process_matches(marker):
                raise DevError("PROCESS_OWNERSHIP_MISMATCH", "The broker leader exited while its owned process group remains.", "Inspect it manually; stop left the marker and socket untouched.")
            if not stop_owned_process(marker):
                raise DevError("PROCESS_OWNERSHIP_MISMATCH", "The broker marker no longer proves process ownership.", "Inspect the environment process manually; no signal was sent.")
            (directory / "broker.process.json").unlink(missing_ok=True)
        elif marker:
            if process_group_alive(marker):
                raise DevError("PROCESS_OWNERSHIP_MISMATCH", "The broker leader exited while its owned process group remains.", "Inspect it manually; no marker or socket was removed.")
            if (not metadata or marker.get("identity") != metadata.get("environment_identity")
                    or marker.get("build_id") != metadata.get("build_id")
                    or marker.get("environment_id") != metadata.get("environment_id")
                    or marker.get("mode") != metadata.get("mode", "synthetic")):
                raise DevError("PROCESS_OWNERSHIP_MISMATCH", "The stale process marker does not belong to this environment.", "Inspect it manually; no marker was removed.")
            (directory / "broker.process.json").unlink(missing_ok=True)
        if broker_lock_is_held(directory / "broker.lock"):
            raise DevError("ENVIRONMENT_ALREADY_RUNNING", "The broker still holds this environment's lifetime lock.", "Inspect the broker owner; no socket or journal was removed.")
        socket_path = runtime_socket_path(name) if metadata else directory / "broker.sock"
        if socket_path.exists() or socket_path.is_symlink():
            if socket_is_active(socket_path):
                raise DevError("ENVIRONMENT_ALREADY_RUNNING", "The selected socket still accepts connections.", "Inspect the broker owner; no socket was removed.")
            if not same_owned_socket(directory, name, metadata):
                raise DevError("BROKER_SOCKET_OWNERSHIP_MISMATCH", "The socket is not the socket recorded by this environment.", "Inspect it manually; it was left untouched.")
            socket_path.unlink()
        return {"ok": True, "status": "stopped", "environment": name,
                "mode": metadata.get("mode", "synthetic") if metadata else "synthetic"}


def reset(name: str) -> dict:
    name = validate_name(name)
    directory = env_dir(name)
    with EnvironmentLock(lock_path(name)):
        marker = read_marker(directory / "broker.process.json")
        metadata = read_environment(directory)
        if metadata and not valid_metadata(directory, name, metadata):
            raise DevError("ENVIRONMENT_METADATA_MISMATCH", "The environment metadata does not belong to this worktree and name.", "Inspect the selected directory; reset left it untouched.")
        mode = metadata.get("mode", "synthetic") if metadata else "synthetic"
        # Lifecycle metadata can be stale or absent after an interrupted import.
        # It must never turn retained live credentials/history into resettable
        # synthetic state.
        config_mode = "synthetic"
        config_path = directory / "config.json"
        if config_path.exists() or config_path.is_symlink():
            try:
                if config_path.is_symlink() or config_path.stat().st_size > 1024 * 1024:
                    raise ValueError("unexpected configuration")
                configured = json.loads(config_path.read_text())
                config_mode = configured.get("mode", "synthetic")
                if config_mode not in VALID_MODES:
                    raise ValueError("unknown mode")
            except (OSError, ValueError, AttributeError):
                raise DevError("ENVIRONMENT_METADATA_MISMATCH", "The saved configuration cannot establish a synthetic environment.", "Inspect the selected environment; reset left its state untouched.") from None
        credential_dir = directory / "credentials"
        if mode == "live" or config_mode == "live" or credential_dir.exists() or credential_dir.is_symlink():
            raise DevError("LIVE_RESET_UNSUPPORTED", "Live development environments keep their financial history and cannot be reset by this command.", "Stop the live environment to preserve its journal, or choose a new environment name for another mode.")
        if broker_lock_is_held(directory / "broker.lock"):
            raise DevError("ENVIRONMENT_RUNNING", "The broker lifetime lock is held; reset cannot remove a live journal.", "Stop this environment, then explicitly retry reset.")
        if marker and process_group_alive(marker):
            if (not metadata or marker.get("identity") != metadata.get("environment_identity")
                    or marker.get("build_id") != metadata.get("build_id")
                    or marker.get("environment_id") != metadata.get("environment_id")
                    or marker.get("mode") != metadata.get("mode", "synthetic")):
                raise DevError("PROCESS_OWNERSHIP_MISMATCH", "The live process marker does not match this environment identity.", "Inspect it manually; reset did not signal it.")
            if not process_matches(marker):
                raise DevError("PROCESS_OWNERSHIP_MISMATCH", "The broker leader exited while its owned process group remains.", "Inspect it manually; reset did not remove state.")
            raise DevError("ENVIRONMENT_RUNNING", "A running broker prevents reset so its journal cannot be removed during a request.", "Stop this environment, then explicitly retry reset.")
        if directory.exists():
            if marker:
                if process_group_alive(marker):
                    raise DevError("PROCESS_OWNERSHIP_MISMATCH", "The broker leader exited while its owned process group remains.", "Inspect it manually; reset left state untouched.")
                if (not metadata or marker.get("identity") != metadata.get("environment_identity")
                        or marker.get("build_id") != metadata.get("build_id")
                        or marker.get("environment_id") != metadata.get("environment_id")
                        or marker.get("mode") != metadata.get("mode", "synthetic")):
                    raise DevError("PROCESS_OWNERSHIP_MISMATCH", "The stale process marker does not belong to this environment.", "Inspect it manually; reset left it untouched.")
                marker_path = directory / "broker.process.json"
                marker_path.unlink(missing_ok=True)
            sock = runtime_socket_path(name) if metadata else directory / "broker.sock"
            if sock.exists() or sock.is_symlink():
                if socket_is_active(sock):
                    raise DevError("ENVIRONMENT_RUNNING", "The selected socket still accepts connections; reset left state untouched.", "Stop the broker and retry reset.")
                if not same_owned_socket(directory, name, metadata):
                    raise DevError("BROKER_SOCKET_OWNERSHIP_MISMATCH", "Reset refused an unowned socket in the selected environment.", "Inspect it manually; reset left the environment untouched.")
                sock.unlink()
            # Ensure the selected directory itself is a plain child of our root.
            root = state_root() / worktree_key()
            if directory.parent != root or directory.is_symlink():
                raise DevError("ENVIRONMENT_PATH_INVALID", "The selected environment path is not a safe owned directory.", "Inspect the state path and retry after repairing it.")
            shutil.rmtree(directory)
        return {"ok": True, "status": "reset", "environment": name, "mode": mode}


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Run isolated synthetic or disposable live Fin development environments.")
    parser.add_argument("--json", action="store_true", help="Emit one JSON result for lifecycle commands.")
    sub = parser.add_subparsers(dest="action", required=True)
    for action in ("start", "import", "cli", "stop", "reset"):
        command = sub.add_parser(action)
        command.add_argument("name")
        if action == "start":
            command.add_argument("--mode", choices=sorted(VALID_MODES), default="synthetic")
            command.add_argument("--fixture", choices=sorted(VALID_FIXTURES),
                                 help="Select a deterministic, in-memory synthetic adapter rehearsal fixture.")
        elif action == "import":
            command.add_argument("--credentials-file", required=True, metavar="PATH")
            command.add_argument("--provider", choices=("binance", "evm", "solana"),
                                 help="Import only credentials for this provider from a mixed .env file.")
            command.add_argument("--discover-identities", action="store_true",
                                 help="Use authenticated read-only Binance calls to discover and verify account identities.")
        if action == "cli":
            command.add_argument("args", nargs=argparse.REMAINDER, help="Arguments passed to this worktree's CLI.")
    options = parser.parse_args(argv)
    machine = options.json or (options.action == "cli" and "--json" in options.args)
    old_handlers = {}
    if threading.current_thread() is threading.main_thread():
        def cancel_signal(_signum, _frame):
            raise KeyboardInterrupt()
        for signum in (signal.SIGTERM, signal.SIGHUP):
            old_handlers[signum] = signal.getsignal(signum)
            signal.signal(signum, cancel_signal)
    try:
        if options.action == "start":
            result = start(options.name, options.json, options.mode, options.fixture)
        elif options.action == "import":
            result = import_credentials(options.name, options.credentials_file,
                                        options.discover_identities, options.provider)
        elif options.action == "stop":
            result = stop(options.name)
        elif options.action == "reset":
            result = reset(options.name)
        else:
            args = list(options.args)
            if args and args[0] == "--":
                args.pop(0)
            command, environment = cli_invocation(options.name, args)
            # One foreground process owns output and cancellation. A Python
            # parent must not emit a second error beside the Rust CLI's JSON.
            os.chdir(REPO)
            try:
                os.execvpe(command[0], command, environment)
            except OSError as cause:
                raise DevError("DEV_CLI_UNAVAILABLE", "The environment CLI could not be started.",
                               "Stop and start this environment to rebuild its binaries.") from cause
    except (DevError, KeyboardInterrupt) as caught:
        if isinstance(caught, KeyboardInterrupt):
            error = DevError("DEVELOPMENT_COMMAND_CANCELLED", "The selected development command was cancelled.", "Retry it when ready; persistent environment state was preserved.")
        else:
            error = caught
        result = {"ok": False, "error": {"code": error.code, "stage": "development", "message": str(error), "next_action": error.next_step}}
        if machine:
            print(json.dumps(result, separators=(",", ":")))
        else:
            print(f"✗ {error}\n  {error.next_step}", file=sys.stderr)
        return 1
    finally:
        for signum, handler in old_handlers.items():
            signal.signal(signum, handler)
    if options.json:
        print(json.dumps(result, separators=(",", ":")))
    elif result["status"] == "running":
        label = "Live development" if result.get("mode") == "live" else "Synthetic"
        print(f"✓ {label} environment {result['environment']} is running ({result['buildId'][:12]}).")
    elif result["status"] == "stopped":
        print(f"✓ {result.get('mode', 'synthetic').capitalize()} environment {result['environment']} stopped. Its journal and configuration are preserved.")
    elif result["status"] == "imported":
        print("✓ Development credentials imported.")
    else:
        mode_label = "Synthetic" if result.get("mode", "synthetic") == "synthetic" else "Live development"
        print(f"✓ {mode_label} environment {result['environment']} was reset.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
