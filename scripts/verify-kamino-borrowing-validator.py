#!/usr/bin/env python3
"""Run the full CLI borrowing lifecycle on an owned, isolated Solana validator.

Only public reads reach mainnet. Programs and market configuration are cloned;
wallet funding is synthetic. Oracle age limits alone are extended in the local
reserve snapshots so frozen public oracle values remain usable during testing.
"""
from __future__ import annotations

import argparse
import base64
import importlib.util
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location("validator_helpers", ROOT / "scripts/verify-meteora-validator.py")
H = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(H)
EVIDENCE = ROOT / ".context/kamino-borrowing-validator"
MAINNET = "https://api.mainnet-beta.solana.com"
PROGRAM = "KLend2g3cP87fffoy8q1mQqGKjrxjC8boSyAYavgmjD"
FARMS = "FarmsPZpWu9i7Kky8tPN37rs2TpmMrAZrC7S7vJa91Hr"
TOKEN = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"
ZERO = "11111111111111111111111111111111"
RESERVES = ["D6q6wuQSrifJKZYpR1M8R4YawnLDtDsMmWM1NbBmgJ59", "H3t6qZ1JkguCNTi9uzVKqQ7dvt2cum4XiXWom6Gn5e5S"]


def setup():
    EVIDENCE.mkdir(parents=True, exist_ok=True)
    accounts = {}
    maximum_slot = 0

    def fetch(addresses):
        nonlocal maximum_slot
        missing = [a for a in dict.fromkeys(addresses) if a != ZERO and a not in accounts]
        for start in range(0, len(missing), 16):
            page = missing[start:start + 16]
            result = H.rpc(MAINNET, "getMultipleAccounts", [page, {"encoding": "base64", "commitment": "finalized"}])
            maximum_slot = max(maximum_slot, int(result["context"]["slot"]))
            for address, account in zip(page, result["value"]):
                if account is not None:
                    accounts[address] = account

    fetch(RESERVES)
    raw_path = EVIDENCE / "public-reserves.json"
    raw_path.write_text(json.dumps([{"address": r, "account": accounts[r]} for r in RESERVES]))
    dependencies = json.loads(subprocess.check_output([
        "cargo", "run", "--quiet", "-p", "gofin-kamino", "--example", "klend_snapshot_dependencies", "--", str(raw_path)
    ], cwd=ROOT, text=True))
    expected_mints = ["EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v", "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB"]
    if [(r["mint"], r["decimals"]) for r in dependencies["reserves"]] != [(m, 6) for m in expected_mints]:
        raise RuntimeError("Selected stablecoin reserve identity changed")
    fetch(dependencies["dependencies"])
    for reserve in dependencies["reserves"]:
        for farm in [reserve["farm_collateral"], reserve["farm_debt"]]:
            if farm != ZERO:
                data = H.account_data(accounts[farm], farm)
                if len(data) != 8336 or accounts[farm]["owner"] != FARMS:
                    raise RuntimeError("Pinned public farm layout changed")
                fetch([H.account_pubkey(data, offset) for offset in [40, 7256, 7536]])
        # This adjustment is confined to synthetic genesis. Production age
        # validation and adapter code are never changed by this runner.
        data = bytearray(H.account_data(accounts[reserve["address"]], reserve["address"]))
        for offset in dependencies["oracle_age_offsets"]:
            data[offset:offset + 8] = (86400).to_bytes(8, "little")
        accounts[reserve["address"]]["data"] = [base64.b64encode(data).decode(), "base64"]
    keypath = EVIDENCE / "synthetic-wallet.json"
    wallet = subprocess.check_output(["solana-keygen", "pubkey", str(keypath)], text=True).strip() if keypath.exists() else H.synthetic_keypair(keypath)
    accounts[wallet] = {"lamports": 1_000_000_000_000, "data": ["", "base64"], "owner": ZERO, "executable": False, "rentEpoch": 0}
    for reserve in dependencies["reserves"]:
        mint = reserve["mint"]
        ata = H.derive_address(H.ATA, f"pubkey:{wallet}", f"pubkey:{TOKEN}", f"pubkey:{mint}")
        amount = 1000 * 10 ** reserve["decimals"]
        accounts[ata] = {"lamports": 2039280, "data": [base64.b64encode(H.synthetic_token_account(mint, wallet, amount, False, 2039280)).decode(), "base64"], "owner": TOKEN, "executable": False, "rentEpoch": 0}
    files = []
    for index, (address, account) in enumerate(accounts.items()):
        path = EVIDENCE / f"account-{index}.json"
        H.write_genesis_account(path, address, account)
        files.append([address, str(path)])
    manifest = {"slot": maximum_slot, "market": dependencies["reserves"][0]["market"], "reserves": dependencies["reserves"], "wallet": wallet, "wallet_key": str(keypath), "accounts": files, "public_read_only": MAINNET, "synthetic_oracle_max_age_seconds": 86400}
    (EVIDENCE / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    return manifest


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--setup", action="store_true")
    parser.add_argument("--port", type=int, default=19421)
    args = parser.parse_args()
    if args.setup:
        setup()
        print("Saved public clones and synthetic borrowing genesis in", EVIDENCE)
        return 0
    manifest_path = EVIDENCE / "manifest.json"
    manifest = json.loads(manifest_path.read_text())
    url = f"http://127.0.0.1:{args.port}"
    with socket.socket() as probe:
        if probe.connect_ex(("127.0.0.1", args.port)) == 0:
            raise RuntimeError("Requested loopback port is occupied")
    ledger = Path(tempfile.mkdtemp(prefix="gofin-klend-", dir="/tmp"))
    command = ["solana-test-validator", "--ledger", str(ledger), "--url", MAINNET, "--rpc-port", str(args.port), "--faucet-port", str(args.port + 10000), "--gossip-port", str(args.port + 10001), "--dynamic-port-range", f"{args.port + 10002}-{args.port + 10102}", "--warp-slot", str(manifest["slot"] + 100), "--clone-upgradeable-program", PROGRAM, FARMS, TOKEN, "--clone", H.ATA]
    for address, path in manifest["accounts"]:
        command.extend(["--account", address, path])
    (EVIDENCE / "command.json").write_text(json.dumps({"command": command, "submission_endpoint": url}, indent=2) + "\n")
    with (EVIDENCE / "validator.log").open("w") as log:
        process = subprocess.Popen(command, cwd=ROOT, stdout=log, stderr=subprocess.STDOUT)
        try:
            deadline = time.monotonic() + 180
            while time.monotonic() < deadline:
                if process.poll() is not None:
                    raise RuntimeError("Owned validator exited; inspect validator.log")
                try:
                    if H.rpc(url, "getHealth", []) == "ok":
                        break
                except Exception:
                    pass
                time.sleep(1)
            else:
                raise RuntimeError("Owned validator did not become healthy")
            env = {**os.environ, "GOFIN_KLEND_VALIDATOR_RPC": url, "GOFIN_KLEND_VALIDATOR_MANIFEST": str(manifest_path), "GOFIN_KLEND_VALIDATOR_EVIDENCE": str(EVIDENCE / "execution.json")}
            with (EVIDENCE / "tests.log").open("w") as output:
                result = subprocess.run(["cargo", "test", "-p", "gofin-broker", "--test", "kamino_borrowing_validator", "--", "--ignored", "--nocapture", "--test-threads=1"], cwd=ROOT, env=env, stdout=output, stderr=subprocess.STDOUT)
            print((EVIDENCE / "tests.log").read_text())
            return result.returncode
        finally:
            process.terminate()
            try:
                process.wait(timeout=20)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
            shutil.rmtree(ledger)


if __name__ == "__main__":
    raise SystemExit(main())
