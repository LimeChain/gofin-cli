#!/usr/bin/env python3
"""Own one isolated Anvil process and invoke the guarded Rust fork harness."""
from __future__ import annotations

import json
import os
from pathlib import Path
import signal
import subprocess
import tempfile
import sys
import time
from typing import Any
from urllib.request import ProxyHandler, Request, build_opener

from fin_dev_resources import allocate_loopback, process_matches, start_owned_process, write_marker
import shutil

FORK_BLOCK = 26_099_326
FORK_HASH = "0xe24db6a0bd48918550f6a7c35c0e129172816d1f6641ac16a37459627551f7bf"
FORK_URL = "https://eth.drpc.org"
ALLOCATE_ATTEMPTS = 8
READY_ATTEMPTS = 30


def rpc(endpoint: str, method: str) -> dict[str, Any]:
    request = Request(
        endpoint,
        data=json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": []}).encode(),
        headers={"Content-Type": "application/json"},
    )
    with build_opener(ProxyHandler({})).open(request, timeout=1) as response:
        payload = json.loads(response.read())
    if payload.get("jsonrpc") != "2.0" or payload.get("id") != 1 or "error" in payload:
        raise ValueError("RPC returned an invalid or failed response")
    result = payload.get("result")
    if not isinstance(result, dict):
        raise ValueError("RPC result is not an object")
    return result


def valid_metadata(metadata: dict[str, Any]) -> bool:
    fork = metadata.get("forkedNetwork")
    return (
        isinstance(metadata.get("instanceId"), str)
        and metadata["instanceId"].startswith("0x")
        and len(metadata["instanceId"]) == 66
        and str(metadata.get("clientVersion", "")).lower().startswith("anvil/")
        and metadata.get("chainId") == 1
        and isinstance(fork, dict)
        and fork.get("forkBlockNumber") == FORK_BLOCK
        and fork.get("forkBlockHash") == FORK_HASH
    )


def child_owns_listener(pid: int, port: int) -> bool:
    """Fail closed unless the spawned PID itself owns the selected listener."""
    lsof = shutil.which("lsof")
    if not lsof:
        return False
    result = subprocess.run(
        [lsof, "-nP", "-a", "-p", str(pid), "-iTCP:%d" % port, "-sTCP:LISTEN"],
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        check=False,
    )
    return result.returncode == 0 and any(
        line.rstrip().endswith(f":{port} (LISTEN)") for line in result.stdout.splitlines()[1:]
    )


def wait_ready(endpoint: str, owned: Any) -> dict[str, Any] | None:
    port = int(endpoint.rsplit(":", 1)[1])
    for _ in range(READY_ATTEMPTS):
        if owned.process.poll() is not None or not process_matches(owned.marker):
            return None
        if not child_owns_listener(owned.process.pid, port):
            time.sleep(1)
            continue
        try:
            metadata = rpc(endpoint, "anvil_metadata")
            # A metadata reply alone never establishes ownership. The marker
            # and live child are checked both before and after the RPC.
            if (
                process_matches(owned.marker)
                and owned.process.poll() is None
                and child_owns_listener(owned.process.pid, port)
                and valid_metadata(metadata)
            ):
                return metadata
        except Exception:
            pass
        time.sleep(1)
    return None


def verify(source: Path, anvil: Path) -> int:
    work = source / "work"
    work.mkdir(mode=0o700, exist_ok=True)
    work.chmod(0o700)
    evidence = Path(tempfile.mkdtemp(prefix="rust-fork.", dir=work))
    evidence.chmod(0o700)
    print(f"Fork evidence: {evidence}", flush=True)

    compile_result = subprocess.run(
        ["cargo", "test", "--locked", "--offline", "-p", "fin-broker", "--test", "fork_roundtrip", "--no-run"],
        cwd=source,
    )
    if compile_result.returncode:
        print("Fork harness compilation failed; no Anvil fixture was started.", file=sys.stderr)
        return compile_result.returncode

    env = os.environ.copy()
    env.pop("FIN_FORK_RPC_URL", None)
    env.pop("FIN_FORK_INSTANCE_ID", None)
    last_issue = "Unable to start an owned Anvil process."
    for attempt in range(ALLOCATE_ATTEMPTS):
        lease = allocate_loopback()
        endpoint, port = lease.url, lease.port
        lease.release()
        node_log = evidence / f"anvil-{attempt + 1}.log"
        marker_path = evidence / f"anvil-{attempt + 1}.owner.json"
        command = [
            str(anvil), "--host", "127.0.0.1", "--port", str(port), "--chain-id", "1",
            "--fork-url", FORK_URL, "--fork-block-number", str(FORK_BLOCK),
            "--timestamp", "1790880107", "--no-cors",
        ]
        try:
            owned = start_owned_process(command, log_path=node_log, marker_path=marker_path, env=env)
        except (OSError, RuntimeError):
            # A raced bind may exit before its PID can be recorded. The shared
            # launcher reaps its own failed child; retry a separately leased port.
            last_issue = f"Anvil exited before ownership was established on port {port}; inspect {node_log}."
            continue
        test_cleanup_complete = True
        try:
            metadata = wait_ready(endpoint, owned)
            if metadata is None:
                last_issue = f"Anvil did not establish the pinned fork on owned port {port}."
                remains_live = owned.process.poll() is None
                # A bind race or immediate startup failure may retry at another
                # port. A live owned node with unavailable archive data is a real
                # fixture failure and should retain its log instead of looping.
                if remains_live:
                    break
                continue

            env["FIN_FORK_RPC_URL"] = endpoint
            env["FIN_FORK_INSTANCE_ID"] = metadata["instanceId"]
            owned.marker["endpoint"] = endpoint
            owned.marker["instance_id"] = metadata["instanceId"]
            write_marker(marker_path, owned.marker)
            (evidence / "fork.json").write_text(
                json.dumps({"endpoint": endpoint, "metadata": metadata}, indent=2) + "\n",
                encoding="utf-8",
            )
            print(f"Owned fork instance {metadata['instanceId']} at {endpoint}", flush=True)
            test_owned = start_owned_process(
                ["cargo", "test", "--manifest-path", str(source / "Cargo.toml"), "--locked", "--offline", "-p", "fin-broker", "--test", "fork_roundtrip", "--", "--ignored", "--nocapture"],
                log_path=evidence / "test.log",
                marker_path=evidence / "test.owner.json",
                env=env,
            )
            test_cleanup_complete = False
            try:
                while test_owned.process.poll() is None:
                    time.sleep(0.2)
                test_status = test_owned.process.returncode
            finally:
                # Stops only this invocation's cargo process group before its
                # Anvil fixture is stopped, including cancellation paths.
                test_cleanup_complete = test_owned.stop()
            if not test_cleanup_complete:
                print("Test process ownership is unresolved; the node and markers were retained for inspection.", file=sys.stderr)
                return 1
            print((evidence / "test.log").read_text(errors="replace"), end="")
            if test_status:
                print("Fork verification failed. Node disposal does not prove financial restoration.", file=sys.stderr)
            return test_status
        finally:
            if test_cleanup_complete:
                if not owned.stop():
                    owned.detach()
                    raise RuntimeError("Anvil cleanup ownership is unresolved; inspect the retained process marker.")
            else:
                owned.detach()
                print("Anvil was retained because the test process group may still be active.", file=sys.stderr)

    print(f"{last_issue} Evidence: {evidence}", file=sys.stderr)
    print("No fork test passed.", file=sys.stderr)
    return 1


def main() -> int:
    source = Path(__file__).resolve().parent.parent
    anvil_value = os.environ.get("FIN_ANVIL_PATH", str(Path.home() / ".foundry/bin/anvil"))
    anvil = Path(anvil_value)
    if not anvil.is_absolute():
        print("FIN_ANVIL_PATH must be an absolute Anvil executable path.", file=sys.stderr)
        return 1
    if not anvil.is_file() or not os.access(anvil, os.X_OK):
        print("Install Foundry Anvil separately; the fork runner does not download it.", file=sys.stderr)
        return 1
    if shutil.which("cargo") is None:
        print("Rust cargo is required to build the fork harness.", file=sys.stderr)
        return 1
    if shutil.which("lsof") is None:
        print("The fork runner requires lsof to prove the selected listener belongs to its Anvil process.", file=sys.stderr)
        return 1
    def cancel(_signum: int, _frame: Any) -> None:
        raise KeyboardInterrupt

    signal.signal(signal.SIGTERM, cancel)
    signal.signal(signal.SIGHUP, cancel)
    try:
        return verify(source, anvil)
    except KeyboardInterrupt:
        print("Fork verification cancelled; inspect any reported retained ownership markers.", file=sys.stderr)
        return 130
    except (OSError, RuntimeError) as error:
        print(f"Fork verification stopped: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
