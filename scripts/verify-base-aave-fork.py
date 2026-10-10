#!/usr/bin/env python3
"""Own one Base OP Anvil fork, pinned by a no-secret public read; never use installed credentials."""
from __future__ import annotations
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import tempfile
import time
from urllib.request import ProxyHandler, Request, build_opener
from fin_dev_resources import allocate_loopback, process_matches, start_owned_process, write_marker

SOURCE = Path(__file__).resolve().parent.parent
PUBLIC_RPC = "https://mainnet.base.org"
TEST = "deployed_base_aave_six_actions_restore_positions_and_allowance"


def rpc(endpoint: str, method: str, params: list) -> object:
    request = Request(endpoint, data=json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).encode(), headers={"Content-Type": "application/json"})
    with build_opener(ProxyHandler({})).open(request, timeout=20) as response:
        result = json.load(response)
    if result.get("jsonrpc") != "2.0" or result.get("id") != 1 or "error" in result or "result" not in result:
        raise RuntimeError(f"Required read failed: {method}")
    return result["result"]


def listener_owned(pid: int, port: int) -> bool:
    result = subprocess.run(["lsof", "-nP", "-a", "-p", str(pid), "-iTCP:%d" % port, "-sTCP:LISTEN"], capture_output=True, text=True, check=False)
    return result.returncode == 0 and any(line.rstrip().endswith(f":{port} (LISTEN)") for line in result.stdout.splitlines()[1:])


def verify() -> int:
    anvil = Path(os.environ.get("FIN_ANVIL_PATH", str(Path.home() / ".foundry/bin/anvil")))
    if not anvil.is_absolute() or not anvil.is_file() or not os.access(anvil, os.X_OK) or not shutil.which("cargo") or not shutil.which("lsof"):
        raise RuntimeError("Existing absolute Anvil binary, cargo and lsof are required; this runner installs nothing.")
    work = SOURCE / "work"
    work.mkdir(exist_ok=True)
    evidence = Path(tempfile.mkdtemp(prefix="base-aave-fork.", dir=work))
    print(f"Base Aave fork evidence: {evidence}", flush=True)
    probe = {"endpoint": PUBLIC_RPC, "operations": ["eth_chainId", "eth_getBlockByNumber(latest)"], "purpose": "Pin independently owned Base Anvil and verify deployed Aave revision compatibility.", "fin_gap": "Fin exposes no deployed Aave token revision or fork lifecycle read; these no-secret read-only direct RPC calls are test orchestration, never writes.", "live_writes": False}
    # Use the application's Rust transport for the public pin. Some public
    # gateways reject Python urllib while the reviewed Fin HTTP client works.
    readiness = subprocess.run(["cargo", "test", "--locked", "--offline", "-p", "fin-ethereum", "--test", "base_aave_read_only", "--", "--ignored", "--exact", "deployed_base_aave_revision_and_provider_readiness", "--nocapture"], cwd=SOURCE, capture_output=True, text=True)
    (evidence / "readiness.log").write_text(readiness.stdout + readiness.stderr)
    try:
        if readiness.returncode:
            raise RuntimeError("Rust public Base readiness read failed; inspect readiness.log.")
        lines = [line.removeprefix("BASE_AAVE_PIN=") for line in readiness.stdout.splitlines() if line.startswith("BASE_AAVE_PIN=")]
        if len(lines) != 1:
            raise RuntimeError("Exactly one checked public Base pin is required.")
        pin = json.loads(lines[0])
        height, block_hash = pin["block"], pin["hash"]
        if pin["chain_id"] != 8453 or height <= 0 or len(block_hash) != 66 or not block_hash.startswith("0x"):
            raise RuntimeError("Public pin lacks exact full Base block identity.")
        int(block_hash[2:], 16)
        probe.update(pin)
    except Exception as error:
        probe["failure"] = str(error)
        (evidence / "pin.json").write_text(json.dumps(probe, indent=2) + "\n")
        print(f"Base pin unavailable: {error}. No fork or financial operation ran.", file=sys.stderr)
        return 1
    (evidence / "pin.json").write_text(json.dumps(probe, indent=2) + "\n")
    compiled = subprocess.run(["cargo", "test", "--locked", "--offline", "-p", "fin-ethereum", "--test", "base_aave_fork", "--no-run"], cwd=SOURCE)
    if compiled.returncode:
        print("Base harness compilation failed; no fork was started.", file=sys.stderr)
        return compiled.returncode
    lease = allocate_loopback()
    endpoint, port = lease.url, lease.port
    lease.release()
    env = os.environ.copy()
    for key in list(env):
        if key.startswith("FIN_BASE_AAVE_FORK_"):
            env.pop(key)
    command = [str(anvil), "--host", "127.0.0.1", "--port", str(port), "--chain-id", "8453", "--optimism", "--fork-url", PUBLIC_RPC, "--fork-block-number", str(height), "--timestamp", str(pin["timestamp"]), "--no-cors"]
    owned = start_owned_process(command, log_path=evidence / "anvil.log", marker_path=evidence / "anvil.owner.json", env=env)
    test_stopped = True
    try:
        metadata = None
        for _ in range(30):
            if owned.process.poll() is not None or not process_matches(owned.marker):
                break
            if listener_owned(owned.process.pid, port):
                try:
                    candidate = rpc(endpoint, "anvil_metadata", [])
                    fork = candidate.get("forkedNetwork", {})
                    if candidate.get("chainId") == 8453 and fork.get("forkBlockNumber") == height and fork.get("forkBlockHash") == block_hash and isinstance(candidate.get("instanceId"), str) and listener_owned(owned.process.pid, port) and process_matches(owned.marker):
                        metadata = candidate
                        break
                except (OSError, RuntimeError, ValueError, AttributeError):
                    pass
            time.sleep(1)
        if metadata is None:
            print("Owned Base fork did not become ready; no test passed.", file=sys.stderr)
            return 1
        env.update({"FIN_BASE_AAVE_FORK_RPC_URL": endpoint, "FIN_BASE_AAVE_FORK_INSTANCE_ID": metadata["instanceId"], "FIN_BASE_AAVE_FORK_BLOCK": str(height), "FIN_BASE_AAVE_FORK_HASH": block_hash})
        owned.marker.update({"endpoint": endpoint, "instance_id": metadata["instanceId"]})
        write_marker(evidence / "anvil.owner.json", owned.marker)
        (evidence / "fork.json").write_text(json.dumps(metadata, indent=2) + "\n")
        test_owned = start_owned_process(["cargo", "test", "--locked", "--offline", "-p", "fin-ethereum", "--test", "base_aave_fork", "--", "--ignored", "--exact", TEST, "--nocapture"], log_path=evidence / "test.log", marker_path=evidence / "test.owner.json", env=env)
        test_stopped = False
        try:
            while test_owned.process.poll() is None:
                time.sleep(0.2)
            status = test_owned.process.returncode
        finally:
            test_stopped = test_owned.stop()
        print((evidence / "test.log").read_text(errors="replace"), end="")
        if not test_stopped:
            print("Test process ownership unresolved; node retained.", file=sys.stderr)
            return 1
        if status:
            print("Base fork failed. Stopping Anvil does not establish restoration or successful settlement.", file=sys.stderr)
        return status
    finally:
        if test_stopped:
            if not owned.stop():
                owned.detach()
                raise RuntimeError("Anvil ownership cleanup unresolved; inspect retained marker.")
        else:
            owned.detach()


def main() -> int:
    def cancel(_signum, _frame):
        raise KeyboardInterrupt
    signal.signal(signal.SIGTERM, cancel)
    signal.signal(signal.SIGHUP, cancel)
    try:
        return verify()
    except KeyboardInterrupt:
        print("Base fork cancelled; inspect retained markers.", file=sys.stderr)
        return 130
    except (OSError, RuntimeError, ValueError) as error:
        print(f"Base fork stopped: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
