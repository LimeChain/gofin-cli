#!/usr/bin/env python3
"""Native Surfpool lifecycle and funding; no external RPC or account fixtures."""
from __future__ import annotations
import base64
import hashlib
import json
from pathlib import Path
import socket
import subprocess
import time
import urllib.request

UPSTREAM = "https://api.mainnet-beta.solana.com"
SYSTEM = "11111111111111111111111111111111"
TOKEN = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"
ATA = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL"


def save(path, value):
    path.write_text(json.dumps(value, indent=2) + "\n")


class Surfpool:
    def __init__(self, directory, snapshot=None, *, rpc_port=None):
        self.directory = directory
        self.directory.mkdir(mode=0o700)
        # Reserve both ports while selecting them; Surfpool binds after release.
        with socket.socket() as rpc, socket.socket() as ws:
            rpc.bind(("127.0.0.1", rpc_port or 0))
            ws.bind(("127.0.0.1", 0))
            port, ws_port = rpc.getsockname()[1], ws.getsockname()[1]
        self.url = f"http://127.0.0.1:{port}"
        self.command = ["surfpool", "start", "--no-tui", "--no-studio", "--no-deploy",
                        "--host", "127.0.0.1", "--rpc-url", UPSTREAM,
                        "--port", str(port), "--ws-port", str(ws_port),
                        "--slot-time", "100",
                        "--airdrop-amount", "0", "--airdrop-keypair-path",
                        str(directory / "no-airdrop.json"), "--db", ":memory:"]
        if snapshot is not None:
            self.command += ["--snapshot", str(snapshot)]
        self.process = None
        self.log = None

    def rpc(self, method, params):
        # Python never connects to the public datasource; Surfpool owns cloning.
        request = urllib.request.Request(self.url, data=json.dumps({
            "jsonrpc": "2.0", "id": 1, "method": method, "params": params,
        }).encode(), headers={"Content-Type": "application/json"})
        with urllib.request.urlopen(request, timeout=90) as response:
            value = json.load(response)
        if "error" in value:
            raise RuntimeError(f"Local Surfpool {method}: {value['error']}")
        return value["result"]

    def __enter__(self):
        save(self.directory / "command.json", self.command)
        self.log = (self.directory / "surfpool.log").open("w")
        self.process = subprocess.Popen(self.command, cwd=self.directory,
                                        stdout=self.log, stderr=subprocess.STDOUT)
        try:
            deadline = time.monotonic() + 240
            while time.monotonic() < deadline:
                if self.process.poll() is not None:
                    raise RuntimeError(f"Surfpool exited; inspect {self.directory / 'surfpool.log'}")
                try:
                    if self.rpc("getBlockHeight", [{"commitment": "finalized"}]) > 0:
                        return self
                except (OSError, ValueError, RuntimeError):
                    pass
                time.sleep(0.5)
            raise RuntimeError(f"Surfpool readiness timed out: {self.directory}")
        except BaseException:
            self.__exit__(None, None, None)
            raise

    def __exit__(self, *_):
        if self.process is not None:
            self.process.terminate()
            try:
                self.process.wait(timeout=15)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait()
        if self.log is not None:
            self.log.close()


def exported_accounts(rpc):
    return rpc("surfnet_exportSnapshot", [{"scope": "network",
        "filter": {"includeProgramAccounts": True}}])["value"]


def cached_rpc(accounts):
    """Rediscover from the local export so lazy reads cannot hide preload failure."""
    def request(method, params):
        if method != "getMultipleAccounts":
            raise RuntimeError(f"Unexpected dependency discovery RPC: {method}")
        values = []
        for address in params[0]:
            account = accounts.get(address)
            values.append(None if account is None else {
                **account, "data": [account["data"], "base64"],
            })
        return {"value": values}
    return request


def set_native_balance(rpc, address, lamports):
    rpc("surfnet_setAccount", [address, {"lamports": lamports, "owner": SYSTEM,
        "executable": False, "rentEpoch": 0, "data": ""}])
    account = rpc("getAccountInfo", [address, {"encoding": "base64"}])["value"]
    if (account is None or account["owner"] != SYSTEM or account["executable"]
            or account["lamports"] != lamports or account["data"] != ["", "base64"]
            or account["rentEpoch"] != 0 or rpc("getBalance", [address])["value"] != lamports):
        raise RuntimeError("Synthetic system account was not initialized exactly")


def wallet(rpc, directory, name, lamports):
    key = directory / f"{name}.json"
    subprocess.run(["solana-keygen", "new", "--no-bip39-passphrase", "--silent",
                    "--outfile", str(key)], check=True, capture_output=True)
    key.chmod(0o600)
    address = subprocess.check_output(["solana-keygen", "pubkey", str(key)], text=True).strip()
    set_native_balance(rpc, address, lamports)
    return address, key


def fund_token(rpc, owner, asset, amount):
    rpc("surfnet_setTokenAccount", [owner, asset["mint"],
        {"amount": amount, "state": "initialized"}, asset["token_program"]])
    result = subprocess.check_output(["solana", "find-program-derived-address",
        "--no-address-labels", "--output", "json", ATA, f"pubkey:{owner}",
        f"pubkey:{asset['token_program']}", f"pubkey:{asset['mint']}"], text=True)
    derived = json.loads(result)
    address = derived.get("address") or derived.get("pubkey")
    balance = rpc("getTokenAccountBalance", [address])["value"]
    if balance["amount"] != str(amount) or balance["decimals"] != asset["decimals"]:
        raise RuntimeError("Synthetic token balance was not initialized exactly")
    account = rpc("getAccountInfo", [address, {"encoding": "jsonParsed"}])["value"]
    if account is None or account["owner"] != asset["token_program"] or account["executable"]:
        raise RuntimeError("Synthetic token account has an unexpected program owner")
    parsed = account["data"]["parsed"]
    info = parsed["info"]
    if (parsed["type"] != "account" or info["mint"] != asset["mint"]
            or info["owner"] != owner or info["state"] != "initialized"
            or info["tokenAmount"]["amount"] != str(amount)
            or info["tokenAmount"]["decimals"] != asset["decimals"]
            or "delegate" in info or "closeAuthority" in info):
        raise RuntimeError("Synthetic token account fields were not initialized exactly")
    native = asset["mint"] == "So11111111111111111111111111111111111111112"
    if info["isNative"] is not native:
        raise RuntimeError("Synthetic token account has an unexpected native-token flag")
    rent = rpc("getMinimumBalanceForRentExemption", [account["space"]])
    if native:
        reserve = int(info["rentExemptReserve"]["amount"])
        if reserve != rent or account["lamports"] != reserve + amount:
            raise RuntimeError("Synthetic WSOL is not backed by exact rent plus token lamports")
    elif account["lamports"] != rent or "rentExemptReserve" in info:
        raise RuntimeError("Synthetic SPL account has unexpected native backing or rent")
    return {"address": address, "owner": owner, **asset, "amount": str(amount),
            "state": info["state"], "is_native": native, "lamports": account["lamports"],
            "rent_exempt_reserve": rent}



def validate_preload(rpc, addresses, programs, evidence_path):
    """Fail closed: ordinary RPC lazy loading must not hide a missing preload."""
    loaded = exported_accounts(rpc)
    missing = sorted(set(addresses) - loaded.keys())
    if missing:
        raise RuntimeError(f"Surfpool did not preload required dependencies: {missing}")
    for address in programs:
        if not loaded[address]["executable"]:
            raise RuntimeError(f"Surfpool did not load executable program {address}")
    save(evidence_path, {
        "mechanism": "Surfpool native address-only snapshot",
        "upstream_context_slot": None,
        "accounts": {address: {"owner": loaded[address]["owner"],
            "executable": loaded[address]["executable"],
            "data_sha256": hashlib.sha256(base64.b64decode(loaded[address]["data"])).hexdigest()}
            for address in addresses},
    })
    return loaded
