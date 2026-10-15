#!/usr/bin/env python3
"""Own one Aave V3 Anvil fork, pinned by a no-secret public read; never use installed credentials."""
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
from gofin_dev_resources import allocate_loopback, process_matches, start_owned_process, write_marker

SOURCE = Path(__file__).resolve().parent.parent
import argparse
parser = argparse.ArgumentParser()
parser.add_argument("--chain", type=int, choices=[1, 8453, 42161], required=True)
parser.add_argument("--pool")
parser.add_argument("--pin", type=Path, help="Recheck and reuse a previously recorded public block/hash pin.")
args = parser.parse_args()
CHAIN = args.chain
PUBLIC_RPC = {1:"https://ethereum-rpc.publicnode.com",8453:"https://mainnet.base.org",42161:"https://arb1.arbitrum.io/rpc"}[CHAIN]
TEST = "deployed_aave_v3_lifecycle_restores_positions_and_allowance"


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
    anvil = Path(os.environ.get("GOFIN_ANVIL_PATH", str(Path.home() / ".foundry/bin/anvil")))
    if not anvil.is_absolute() or not anvil.is_file() or not os.access(anvil, os.X_OK) or not shutil.which("cargo") or not shutil.which("lsof"):
        raise RuntimeError("Existing absolute Anvil binary, cargo and lsof are required; this runner installs nothing.")
    work = SOURCE / "work"
    work.mkdir(exist_ok=True)
    evidence = Path(tempfile.mkdtemp(prefix="aave-v3-fork.", dir=work))
    print(f"Aave V3 fork evidence: {evidence}", flush=True)
    probe = {"endpoint": PUBLIC_RPC, "operations": ["eth_chainId", "eth_getBlockByNumber(latest)"], "purpose": "Pin independently owned exact-network Anvil and verify deployed Aave revision compatibility.", "gofin_gap": "GoFin now exposes token revisions through aave.reserves; it has no fork-control lifecycle; these no-secret read-only direct RPC calls are test orchestration, never writes.", "live_writes": False}
    saved_pin = json.loads(args.pin.read_text()) if args.pin else None
    if saved_pin is not None and (saved_pin.get("chain_id") != CHAIN or not isinstance(saved_pin.get("block"), int) or saved_pin["block"] <= 0 or not isinstance(saved_pin.get("hash"), str)):
        raise RuntimeError("Saved pin lacks the exact selected chain/block/hash.")
    pin_env = {**os.environ,"GOFIN_AAVE_PIN_CHAIN_ID":str(CHAIN)}
    pin_env.pop("GOFIN_AAVE_PIN_BLOCK",None)
    if saved_pin is not None:
        pin_env["GOFIN_AAVE_PIN_BLOCK"] = str(saved_pin["block"])
    # Use the application's Rust transport for the public pin. Some public
    # gateways reject Python urllib while the reviewed GoFin HTTP client works.
    readiness = subprocess.run(["cargo", "test", "--locked", "--offline", "-p", "gofin-ethereum", "--test", "aave_fork_pin", "--", "--ignored", "--exact", "public_aave_fork_pin", "--nocapture"], cwd=SOURCE, capture_output=True, text=True, env=pin_env)
    (evidence / "readiness.log").write_text(readiness.stdout + readiness.stderr)
    try:
        if readiness.returncode:
            raise RuntimeError("Rust public chain readiness read failed; inspect readiness.log.")
        lines = [line.removeprefix("AAVE_FORK_PIN=") for line in readiness.stdout.splitlines() if line.startswith("AAVE_FORK_PIN=")]
        if len(lines) != 1:
            raise RuntimeError("Exactly one checked public Aave fork pin is required.")
        pin = json.loads(lines[0])
        height, block_hash = pin["block"], pin["hash"]
        if pin["chain_id"] != CHAIN or height <= 0 or len(block_hash) != 66 or not block_hash.startswith("0x"):
            raise RuntimeError("Public pin lacks exact full network block identity.")
        int(block_hash[2:], 16)
        if saved_pin is not None and (height != saved_pin["block"] or block_hash != saved_pin["hash"]):
            raise RuntimeError("Saved public block/hash no longer matches canonical RPC evidence.")
        probe.update(pin)
    except Exception as error:
        probe["failure"] = str(error)
        (evidence / "pin.json").write_text(json.dumps(probe, indent=2) + "\n")
        print(f"Aave fork pin unavailable: {error}. No fork or financial operation ran.", file=sys.stderr)
        return 1
    (evidence / "pin.json").write_text(json.dumps(probe, indent=2) + "\n")
    compiled = subprocess.run(["cargo", "test", "--locked", "--offline", "-p", "gofin-ethereum", "--test", "aave_v3_fork", "--no-run"], cwd=SOURCE)
    if compiled.returncode:
        print("Aave harness compilation failed; no fork was started.", file=sys.stderr)
        return compiled.returncode
    lease = allocate_loopback()
    endpoint, port = lease.url, lease.port
    lease.release()
    env = os.environ.copy()
    env["GOFIN_AAVE_PIN_CHAIN_ID"] = str(CHAIN)
    for key in list(env):
        if key.startswith("GOFIN_AAVE_FORK_"):
            env.pop(key)
    command = [str(anvil), "--host", "127.0.0.1", "--port", str(port), "--chain-id", str(CHAIN), *(["--optimism"] if CHAIN == 8453 else []), "--fork-url", PUBLIC_RPC, "--fork-block-number", str(height), "--timestamp", str(pin["timestamp"]), "--no-cors"]
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
                    if candidate.get("chainId") == CHAIN and fork.get("forkBlockNumber") == height and fork.get("forkBlockHash") == block_hash and isinstance(candidate.get("instanceId"), str) and listener_owned(owned.process.pid, port) and process_matches(owned.marker):
                        metadata = candidate
                        break
                except (OSError, RuntimeError, ValueError, AttributeError):
                    pass
            time.sleep(1)
        if metadata is None:
            print("Owned Aave V3 fork did not become ready; no test passed.", file=sys.stderr)
            return 1
        env["GOFIN_AAVE_FORK_CHAIN_ID"] = str(CHAIN)
        if args.pool: env["GOFIN_AAVE_FORK_POOL"] = args.pool.lower()
        env.update({"GOFIN_AAVE_FORK_RPC_URL": endpoint, "GOFIN_AAVE_FORK_INSTANCE_ID": metadata["instanceId"], "GOFIN_AAVE_FORK_BLOCK": str(height), "GOFIN_AAVE_FORK_HASH": block_hash})
        owned.marker.update({"endpoint": endpoint, "instance_id": metadata["instanceId"]})
        write_marker(evidence / "anvil.owner.json", owned.marker)
        (evidence / "fork.json").write_text(json.dumps(metadata, indent=2) + "\n")
        test_owned = start_owned_process(["cargo", "test", "--locked", "--offline", "-p", "gofin-ethereum", "--test", "aave_v3_fork", "--", "--ignored", "--exact", TEST, "--nocapture"], log_path=evidence / "test.log", marker_path=evidence / "test.owner.json", env=env)
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
            print("Aave V3 fork failed. Stopping Anvil does not establish restoration or successful settlement.", file=sys.stderr)
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
        print("Aave V3 fork cancelled; inspect retained markers.", file=sys.stderr)
        return 130
    except (OSError, RuntimeError, ValueError) as error:
        print(f"Aave V3 fork stopped: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
