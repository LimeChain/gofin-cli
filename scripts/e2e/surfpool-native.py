#!/usr/bin/env python3
"""Native Surfpool lifecycle and funding; no external RPC or account fixtures."""
from __future__ import annotations
import base64
import hashlib
import json
import re
from pathlib import Path
import socket
import subprocess
import time
import urllib.parse
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


# Pinned Surfpool 1.5.0 subtracts 31 from its processed absolute slot.
FINALIZATION_SLOT_DISTANCE = 31
# The owned instances use 100 ms slots. Leave 3.2 seconds of forward room for
# the native two-second command acknowledgement and its internally produced block.
TIME_TRAVEL_RACE_SLOTS = 32


class LocalSurfpoolRpcError(RuntimeError):
    def __init__(self, error):
        self.error = error
        super().__init__(f"Local Surfpool RPC failed: {error}")


class NoRpcRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *_args, **_kwargs):
        raise RuntimeError("Local Surfpool RPC must not redirect")


def local_surfpool_rpc(url, method, params, timeout):
    parsed = urllib.parse.urlsplit(url)
    if (parsed.scheme != "http" or parsed.hostname not in {"127.0.0.1", "::1"}
            or parsed.port is None or parsed.username or parsed.password
            or parsed.path not in {"", "/"} or parsed.query or parsed.fragment):
        raise ValueError("Finalization time travel requires an explicit loopback Surfpool URL")
    request = urllib.request.Request(url, data=json.dumps({
        "jsonrpc": "2.0", "id": 1, "method": method, "params": params,
    }).encode(), headers={"Content-Type": "application/json"})
    # Ignore proxy environment variables and refuse redirects away from the
    # owned local node (including the JLP harness's local oracle proxy).
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRpcRedirect())
    with opener.open(request, timeout=timeout) as response:
        value = json.load(response)
    if "error" in value:
        raise LocalSurfpoolRpcError(value["error"])
    return value["result"]


def advance_finalized_slot(url, receipt_slot, *, timeout=10.0):
    """Advance an owned Surfpool clock, then prove finalized > receipt_slot.

    Only call after execution has completed. This does not confirm a transaction
    or replace its receipt, balance or settlement checks. Never use concurrently
    with another clock controller on the same instance.
    """
    if type(receipt_slot) is not int or receipt_slot < 0 or not 0 < timeout <= 60:
        raise ValueError("A nonnegative receipt slot and bounded timeout are required")
    started = time.monotonic()
    deadline = started + timeout
    evidence = {"mode": "time_travel", "receipt_slot": receipt_slot,
                "targets": [], "polls": 0, "past_slot_races": 0}

    def rpc(method, params):
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise TimeoutError("Surfpool did not prove a finalized slot beyond the receipt")
        return local_surfpool_rpc(url, method, params, min(2.0, remaining))

    def slot(commitment):
        value = rpc("getSlot", [{"commitment": commitment}])
        if type(value) is not int or value < 0:
            raise RuntimeError("Surfpool returned an invalid slot")
        if commitment == "finalized":
            evidence["polls"] += 1
        return value

    def clock_timestamp():
        account = rpc("getAccountInfo", ["SysvarC1ock11111111111111111111111111111111",
                                         {"encoding": "base64"}])["value"]
        if account is None:
            raise RuntimeError("Surfpool clock sysvar is missing")
        raw = base64.b64decode(account["data"][0], validate=True)
        if len(raw) != 40:
            raise RuntimeError("Surfpool clock sysvar layout is unexpected")
        return int.from_bytes(raw[32:40], "little", signed=True)

    def finish(finalized):
        evidence.update(finalized_slot=finalized, elapsed_seconds=time.monotonic()-started)
        return evidence

    finalized = slot("finalized")
    evidence["initial_finalized_slot"] = finalized
    if finalized > receipt_slot:
        return finish(finalized)
    previous_processed = None
    for _ in range(3):
        processed = slot("processed")
        if previous_processed is not None and processed < previous_processed:
            raise RuntimeError("Surfpool processed slot regressed during finalization")
        previous_processed = processed
        evidence.setdefault("initial_processed_slot", processed)
        # The clock may have caught up between the first two reads.
        finalized = slot("finalized")
        if finalized > receipt_slot:
            return finish(finalized)
        before_time = clock_timestamp()
        target = max(receipt_slot + FINALIZATION_SLOT_DISTANCE + 1,
                     processed + TIME_TRAVEL_RACE_SLOTS)
        evidence["targets"].append(target)
        try:
            rpc("surfnet_timeTravel", [{"absoluteSlot": target}])
        except LocalSurfpoolRpcError as error:
            # Recalculate only the pinned native rejection of a target overtaken
            # by block production. Other failures, including uncertain timeouts,
            # are errors, not permission to accept an unverified finalization.
            message = error.error.get("data", "")
            match = re.fullmatch(r"Cannot travel to past slot: target=(\d+), current=(\d+)", message) if isinstance(message, str) else None
            if error.error.get("code") != -32603 or not match or int(match[1]) != target or int(match[2]) <= target:
                raise
            evidence["past_slot_races"] += 1
            finalized = slot("finalized")
            if finalized > receipt_slot:
                return finish(finalized)
            continue
        while time.monotonic() < deadline:
            observed = slot("processed")
            if observed < previous_processed:
                raise RuntimeError("Surfpool processed slot regressed after time travel")
            previous_processed = observed
            finalized = slot("finalized")
            if finalized > receipt_slot:
                after_time = clock_timestamp()
                if after_time < before_time:
                    raise RuntimeError("Surfpool timestamp regressed after time travel")
                evidence.update(processed_slot=observed,
                                simulated_timestamp_before=before_time,
                                simulated_timestamp_after=after_time)
                return finish(finalized)
            time.sleep(min(0.05, max(0, deadline-time.monotonic())))
        raise TimeoutError("Surfpool time travel did not prove finalized > receipt slot")
    raise RuntimeError("Surfpool finalization exceeded the bounded clock-race retries")
