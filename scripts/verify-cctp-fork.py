#!/usr/bin/env python3
"""Owned CCTP contract forks; never attach to a remote or pre-existing write node."""
from __future__ import annotations
import argparse
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
URLS = {1: "https://ethereum-rpc.publicnode.com", 42161: "https://arb1.arbitrum.io/rpc", 8453: "https://mainnet.base.org"}


def rpc(endpoint, method, params):
    request = Request(endpoint, data=json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).encode(), headers={"Content-Type": "application/json"})
    with build_opener(ProxyHandler({})).open(request, timeout=20) as response:
        value = json.load(response)
    if value.get("jsonrpc") != "2.0" or value.get("id") != 1 or "error" in value or "result" not in value:
        raise RuntimeError(f"Required owned-fork read failed: {method}")
    return value["result"]


def listener_owned(pid, port):
    result = subprocess.run(["lsof", "-nP", "-a", "-p", str(pid), f"-iTCP:{port}", "-sTCP:LISTEN"], capture_output=True, text=True, check=False)
    return result.returncode == 0 and any(line.rstrip().endswith(f":{port} (LISTEN)") for line in result.stdout.splitlines()[1:])


def start_fork(chain, anvil, evidence):
    evidence.mkdir()
    env = {key: value for key, value in os.environ.items() if not key.startswith("GOFIN_CCTP_FORK_")}
    env["GOFIN_CCTP_PIN_CHAIN_ID"] = str(chain)
    result = subprocess.run(["cargo", "test", "--locked", "--offline", "-p", "gofin-broker", "--test", "cctp_fork", "public_cctp_fork_pin", "--", "--ignored", "--exact", "--nocapture"], cwd=SOURCE, env=env, capture_output=True, text=True)
    (evidence / "pin.log").write_text(result.stdout + result.stderr)
    if result.returncode:
        raise RuntimeError(f"Chain {chain} public pin failed; inspect {evidence / 'pin.log'}. No fork writes ran.")
    pins = [json.loads(line.removeprefix("CCTP_FORK_PIN=")) for line in result.stdout.splitlines() if line.startswith("CCTP_FORK_PIN=")]
    if len(pins) != 1 or pins[0]["chain_id"] != chain:
        raise RuntimeError("Exactly one matching public chain pin is required")
    pin = pins[0]
    if pin["block"] <= 0 or len(pin["hash"]) != 66 or not pin["hash"].startswith("0x"):
        raise RuntimeError("Malformed public chain anchor")
    int(pin["hash"][2:], 16)
    (evidence / "pin.json").write_text(json.dumps(pin, indent=2) + "\n")
    lease = allocate_loopback()
    endpoint, port = lease.url, lease.port
    lease.release()
    node = start_owned_process([str(anvil), "--host", "127.0.0.1", "--port", str(port), "--chain-id", str(chain), *(["--optimism"] if chain == 8453 else []), "--fork-url", URLS[chain], "--fork-block-number", str(pin["block"]), "--timestamp", str(pin["timestamp"]), "--no-cors"], log_path=evidence / "anvil.log", marker_path=evidence / "anvil.owner.json", env=env)
    try:
        metadata = None
        for _ in range(60):
            if node.process.poll() is not None or not process_matches(node.marker):
                break
            if listener_owned(node.process.pid, port):
                try:
                    candidate = rpc(endpoint, "anvil_metadata", [])
                    fork = candidate.get("forkedNetwork", {})
                    if candidate.get("chainId") == chain and fork.get("forkBlockNumber") == pin["block"] and fork.get("forkBlockHash") == pin["hash"] and isinstance(candidate.get("instanceId"), str):
                        metadata = candidate
                        break
                except (OSError, RuntimeError, ValueError):
                    pass
            time.sleep(.5)
        if metadata is None or not listener_owned(node.process.pid, port) or not process_matches(node.marker):
            raise RuntimeError("Owned CCTP fork/archive not ready; no contract result established")
        node.marker.update(endpoint=endpoint, instance_id=metadata["instanceId"])
        write_marker(evidence / "anvil.owner.json", node.marker)
        (evidence / "fork.json").write_text(json.dumps(metadata, indent=2) + "\n")
        return node, {"url": endpoint, "instance": metadata["instanceId"], "pin": pin}
    except BaseException:
        if not node.stop():
            node.detach()
        raise


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--chain", type=int, choices=list(URLS), help="Select source burns; destination forks still cover all three networks.")
    args = parser.parse_args()
    anvil = Path(os.environ.get("GOFIN_ANVIL_PATH", str(Path.home() / ".foundry/bin/anvil")))
    if not anvil.is_absolute() or not anvil.is_file() or not os.access(anvil, os.X_OK) or not shutil.which("cargo") or not shutil.which("lsof"):
        raise RuntimeError("An existing absolute Anvil binary, cargo and lsof are required; nothing is installed")
    (SOURCE / "work").mkdir(exist_ok=True)
    evidence = Path(tempfile.mkdtemp(prefix="cctp-fork.", dir=SOURCE / "work"))
    print(f"CCTP fork evidence: {evidence}", flush=True)
    compiled = subprocess.run(["cargo", "test", "--locked", "--offline", "-p", "gofin-broker", "--test", "cctp_fork", "--no-run"], cwd=SOURCE)
    if compiled.returncode:
        return compiled.returncode
    nodes = []
    test = None
    try:
        fixtures = []
        for chain in URLS:
            node, fixture = start_fork(chain, anvil, evidence / str(chain))
            nodes.append(node)
            fixtures.append(fixture)
        env = {key: value for key, value in os.environ.items() if not key.startswith("GOFIN_CCTP_FORK") and key != "GOFIN_CCTP_SOURCE_CHAIN"}
        env["GOFIN_CCTP_FORKS"] = json.dumps(fixtures)
        if args.chain:
            env["GOFIN_CCTP_SOURCE_CHAIN"] = str(args.chain)
        test = start_owned_process(["cargo", "test", "--locked", "--offline", "-p", "gofin-broker", "--test", "cctp_fork", "deployed_cctp_burn_and_mint_bind_each_supported_destination", "--", "--ignored", "--exact", "--nocapture"], log_path=evidence / "test.log", marker_path=evidence / "test.owner.json", env=env)
        status = test.process.wait()
        print((evidence / "test.log").read_text(errors="replace"), end="")
        return status
    finally:
        if test is not None and not test.stop():
            test.detach()
            for node in nodes:
                node.detach()
            raise RuntimeError("Owned test cleanup unresolved; destination forks retained")
        for node in reversed(nodes):
            if not node.stop():
                node.detach()
                raise RuntimeError("Owned fork cleanup unresolved")



if __name__ == "__main__":
    def cancel(_signal, _frame):
        raise KeyboardInterrupt
    signal.signal(signal.SIGTERM, cancel)
    signal.signal(signal.SIGHUP, cancel)
    try:
        raise SystemExit(main())
    except KeyboardInterrupt:
        print("CCTP fork cancelled; retained evidence identifies owned processes.", file=sys.stderr)
        raise SystemExit(130)
    except (OSError, RuntimeError, ValueError) as error:
        print(f"CCTP fork blocked: {error}", file=sys.stderr)
        raise SystemExit(1)
