#!/usr/bin/env python3
"""Run Meteora adapter tests against public account clones on an isolated validator.

All public RPC requests are reads performed by solana-test-validator while it
builds genesis. Every simulation and send is forced to the loopback validator.
The script never reads .env or a production wallet; its keypairs and account
dumps are synthetic, ignored development fixtures under .context/.
"""

from __future__ import annotations

import argparse
import base64
import hashlib
import json
import os
import shutil
import socket
import subprocess
import sys
import time
import urllib.request
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
EVIDENCE = ROOT / ".context" / "meteora-validator"
MAINNET = "https://api.mainnet-beta.solana.com"
PROGRAMS = [
    "LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo",  # DLMM
    "cpamdpZCGKUy5JxQXB4dcpGPiikHawvSWAd6mEn1sGG",  # DAMM v2
    "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA",  # SPL Token
    "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb",  # Token-2022
]
MEMO = "MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr"
ATA = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL"


def rpc(url: str, method: str, params: list) -> dict:
    body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).encode()
    request = urllib.request.Request(url, data=body, headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(request, timeout=30) as response:
        value = json.load(response)
    if "error" in value:
        raise RuntimeError(f"RPC {method} failed: {value['error']}")
    return value["result"]


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def base58_encode(raw: bytes) -> str:
    alphabet = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz"
    zeros = len(raw) - len(raw.lstrip(b"\0"))
    number = int.from_bytes(raw, "big")
    encoded = ""
    while number:
        number, remainder = divmod(number, 58)
        encoded = alphabet[remainder] + encoded
    return "1" * zeros + encoded


def derive_address(program: str, *seeds: str) -> str:
    result = subprocess.run(
        ["solana", "find-program-derived-address", "--no-address-labels", "--output", "json", program, *seeds],
        cwd=ROOT,
        check=True,
        capture_output=True,
        text=True,
    )
    return json.loads(result.stdout)["address"]


def rpc_accounts(addresses: list[str]) -> tuple[int, list[dict | None]]:
    result = rpc(MAINNET, "getMultipleAccounts", [addresses, {"encoding": "base64", "commitment": "finalized"}])
    values = result.get("value")
    if not isinstance(values, list) or len(values) != len(addresses):
        raise RuntimeError("Public RPC returned an incomplete account page.")
    return int(result["context"]["slot"]), values


def account_data(account: dict, label: str) -> bytes:
    encoded = account.get("data")
    if not isinstance(encoded, list) or len(encoded) != 2 or encoded[1] != "base64":
        raise RuntimeError(f"Public RPC returned malformed {label} account data.")
    return base64.b64decode(encoded[0], validate=True)


def account_pubkey(data: bytes, offset: int) -> str:
    return base58_encode(data[offset : offset + 32])


def synthetic_keypair(path: Path) -> str:
    path.parent.mkdir(parents=True, exist_ok=True)
    subprocess.run(
        ["solana-keygen", "new", "--no-bip39-passphrase", "--silent", "--force", "--outfile", str(path)],
        cwd=ROOT,
        check=True,
        capture_output=True,
        text=True,
    )
    return subprocess.run(
        ["solana-keygen", "pubkey", str(path)], cwd=ROOT, check=True, capture_output=True, text=True
    ).stdout.strip()


def write_genesis_account(path: Path, pubkey: str, account: dict) -> None:
    path.write_text(json.dumps({"pubkey": pubkey, "account": account}, indent=2) + "\n")


def synthetic_token_account(mint: str, owner: str, amount: int, native: bool, rent: int) -> bytes:
    data = bytearray()
    # SPL Token Account layout: mint, owner, amount, delegate, state,
    # native reserve, delegated amount, and close authority.
    def pubkey_bytes(value: str) -> bytes:
        alphabet = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz"
        number = 0
        for char in value:
            number = number * 58 + alphabet.index(char)
        raw = number.to_bytes(32, "big")
        zeros = len(value) - len(value.lstrip("1"))
        return bytes(zeros) + raw[zeros:]

    data.extend(pubkey_bytes(mint))
    data.extend(pubkey_bytes(owner))
    data.extend(amount.to_bytes(8, "little"))
    data.extend(bytes(36))  # no delegate
    data.append(1)  # initialized
    data.extend((1).to_bytes(4, "little") + rent.to_bytes(8, "little") if native else bytes(12))
    data.extend(bytes(8))  # delegated amount
    data.extend(bytes(36))  # no close authority
    if len(data) != 165:
        raise RuntimeError("Synthetic token account layout has an unexpected size.")
    return bytes(data)


def setup_public_fixtures(force: bool) -> None:
    required = ["solana", "solana-keygen"]
    if any(shutil.which(tool) is None for tool in required):
        raise RuntimeError("Fixture setup requires solana and solana-keygen.")
    if EVIDENCE.exists() and any(EVIDENCE.iterdir()) and not force:
        raise RuntimeError("Validator fixtures already exist. Pass --replace-synthetic-fixtures to replace these ignored local files.")
    EVIDENCE.mkdir(parents=True, exist_ok=True)
    # The selected public pools are fixed so validator results can be compared
    # across runs; only account snapshots and initialized nearby arrays change.
    dlmm_pool = "5rCf1DM8LjKTw4YqhnoLcngyZYeNnQqztScTogYHAS6"
    damm_pool = "8Pm2kZpnxD3hoMmt4bjStX2Pw2Z9abpbHzZxMPqxPmie"
    snapshot_slot, pools = rpc_accounts([dlmm_pool, damm_pool])
    if any(account is None for account in pools):
        raise RuntimeError("A selected public Meteora pool is missing from the finalized RPC snapshot.")
    dlmm_data = account_data(pools[0], "DLMM pair")
    damm_data = account_data(pools[1], "DAMM pool")
    if len(dlmm_data) < 904 or len(damm_data) < 1112:
        raise RuntimeError("A selected public Meteora pool has an unsupported account layout.")
    active_bin = int.from_bytes(dlmm_data[8 + 68 : 8 + 72], "little", signed=True)
    active_bin_array = active_bin // 70
    mint_x, mint_y = account_pubkey(dlmm_data, 8 + 80), account_pubkey(dlmm_data, 8 + 112)
    if mint_x != "So11111111111111111111111111111111111111112" or mint_y != "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v":
        raise RuntimeError("Selected DLMM pool no longer matches the validator test's pinned WSOL/USDC pair.")
    reserve_x, reserve_y = account_pubkey(dlmm_data, 8 + 144), account_pubkey(dlmm_data, 8 + 176)
    oracle = derive_address(PROGRAMS[0], "string:oracle", f"pubkey:{dlmm_pool}")
    bitmap = derive_address(PROGRAMS[0], "string:bitmap", f"pubkey:{dlmm_pool}")
    array_indices = range(active_bin_array - 15, active_bin_array + 16)
    array_addresses = {
        index: derive_address(PROGRAMS[0], "string:bin_array", f"pubkey:{dlmm_pool}", f"i64le:{index}")
        for index in array_indices
    }
    bin_slot, bin_accounts = rpc_accounts(list(array_addresses.values()))
    bin_arrays = {
        str(index): address
        for (index, address), account in zip(array_addresses.items(), bin_accounts)
        if account is not None
    }
    if not bin_arrays:
        raise RuntimeError("No initialized DLMM BinArray accounts were found around the active bin.")
    manifest = {
        "rpc": "mainnet-beta public RPC, read-only",
        "snapshot_slot": max(snapshot_slot, bin_slot),
        "dlmm": {
            "program": PROGRAMS[0], "pool": dlmm_pool, "active_bin": active_bin,
            "active_bin_array": active_bin_array, "bin_step": int.from_bytes(dlmm_data[8 + 72 : 8 + 74], "little"),
            "token_x_mint": mint_x, "token_y_mint": mint_y, "reserve_x": reserve_x,
            "reserve_y": reserve_y, "oracle": oracle, "bitmap_extension": bitmap, "bin_arrays": bin_arrays,
        },
        "damm_v2": {
            "program": PROGRAMS[1], "pool": damm_pool,
            "token_a_mint": account_pubkey(damm_data, 8 + 160),
            "token_b_mint": account_pubkey(damm_data, 8 + 192),
            "vault_a": account_pubkey(damm_data, 8 + 224),
            "vault_b": account_pubkey(damm_data, 8 + 256),
        },
    }
    if (
        manifest["damm_v2"]["token_a_mint"] != mint_x
        or manifest["damm_v2"]["token_b_mint"] != mint_y
    ):
        raise RuntimeError("Selected DAMM pool no longer matches the validator test's pinned token ordering.")
    (EVIDENCE / "public-clone-manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")

    identity_rows = {}
    for name in ("wallet", "position-mint", "temporary-wsol"):
        path = EVIDENCE / f"{name}.json"
        identity_rows[name] = {"path": str(path.relative_to(ROOT)), "pubkey": synthetic_keypair(path)}
    wallet = identity_rows["wallet"]["pubkey"]
    mints = [manifest["dlmm"]["token_x_mint"], manifest["dlmm"]["token_y_mint"]]
    mint_slot, mint_accounts = rpc_accounts(mints)
    account_descriptors = []
    for mint, mint_account in zip(mints, mint_accounts):
        if mint_account is None or mint_account["owner"] not in ("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA", "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb"):
            raise RuntimeError("A public pool mint is missing or not owned by SPL Token / Token-2022.")
        token_program = mint_account["owner"]
        ata = derive_address(ATA, f"pubkey:{wallet}", f"pubkey:{token_program}", f"pubkey:{mint}")
        account_descriptors.append((mint, token_program, ata))
    rent = int(rpc(MAINNET, "getMinimumBalanceForRentExemption", [165]))
    for label, (mint, token_program, ata) in zip(("wsol-ata", "usdc-ata"), account_descriptors):
        is_native = mint == "So11111111111111111111111111111111111111112"
        amount = 10_000_000_000 if is_native else 1_000_000_000_000_000
        raw = synthetic_token_account(mint, wallet, amount, is_native, rent)
        write_genesis_account(EVIDENCE / f"{label}.json", ata, {
            "lamports": rent + (amount if is_native else 0), "data": [base64.b64encode(raw).decode(), "base64"],
            "owner": token_program, "executable": False,
            "rentEpoch": 18446744073709551615, "space": len(raw),
        })
        identity_rows[label.replace("-ata", "_ata")] = {"pubkey": ata, "mint": mint, "token_program": token_program}
    write_genesis_account(EVIDENCE / "wallet-account.json", wallet, {
        "lamports": 1_000_000_000_000, "data": ["", "base64"],
        "owner": "11111111111111111111111111111111", "executable": False,
        "rentEpoch": 18446744073709551615, "space": 0,
    })
    (EVIDENCE / "synthetic-identities.json").write_text(json.dumps(identity_rows, indent=2) + "\n")
    (EVIDENCE / "fixture-setup.json").write_text(json.dumps({
        "public_read_slot": max(snapshot_slot, bin_slot, mint_slot),
        "synthetic_genesis": "1,000 SOL wallet; 10 WSOL with matching lamport backing and 1,000,000,000 USDC; local-only random keypairs",
        "submission_target": "loopback solana-test-validator only",
    }, indent=2) + "\n")


def local_clone_addresses(manifest: dict) -> list[str]:
    dlmm = manifest["dlmm"]
    damm = manifest["damm_v2"]
    addresses = [
        dlmm["pool"], dlmm["token_x_mint"], dlmm["token_y_mint"],
        dlmm["reserve_x"], dlmm["reserve_y"], dlmm["oracle"],
        dlmm["bitmap_extension"], *dlmm["bin_arrays"].values(),
        damm["pool"], damm["token_a_mint"], damm["token_b_mint"],
        damm["vault_a"], damm["vault_b"],
    ]
    return list(dict.fromkeys(addresses))


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--port", type=int, default=19001)
    parser.add_argument("--family", choices=("both", "dlmm", "damm"), default="both")
    parser.add_argument("--keep-ledger", action="store_true", help="reuse this port's existing ignored ledger")
    parser.add_argument("--setup", action="store_true", help="fetch bounded public pool fixtures and create synthetic local genesis accounts")
    parser.add_argument("--replace-synthetic-fixtures", action="store_true", help="replace ignored local fixtures when used with --setup")
    args = parser.parse_args()
    if args.setup:
        try:
            setup_public_fixtures(args.replace_synthetic_fixtures)
        except Exception as error:
            raise SystemExit(f"Fixture setup failed: {error}") from error
        print(f"Prepared ignored public-clone and synthetic-genesis fixtures in {EVIDENCE}")
        return 0
    for tool in ("solana-test-validator", "cargo"):
        if shutil.which(tool) is None:
            raise SystemExit(f"Required executable is missing: {tool}")
    manifest_path = EVIDENCE / "public-clone-manifest.json"
    identities_path = EVIDENCE / "synthetic-identities.json"
    if not manifest_path.exists() or not identities_path.exists():
        raise SystemExit("Missing ignored .context validator manifest or synthetic identities. Create these read-only public-clone fixtures first.")
    manifest = json.loads(manifest_path.read_text())
    identities = json.loads(identities_path.read_text())
    # Compatibility with the initial ignored fixture set created before the
    # self-service setup mode existed.
    identities.setdefault("wsol_ata", {"pubkey": "FEtwtGUj88LG9yzfMufDZnjHkngP2H74T5VtV9NVckvM"})
    identities.setdefault("usdc_ata", {"pubkey": "35aT8CH21L2eMmpCaa5LTR5F76EWwppv3eXSSHnFWyjp"})
    for name in ("wallet", "position-mint", "temporary-wsol"):
        key_path = Path(identities[name]["path"])
        if not key_path.is_absolute():
            key_path = ROOT / key_path
        identities[name]["path"] = str(key_path)
        if not key_path.is_file():
            raise SystemExit(f"Missing synthetic keypair: {key_path}")
    genesis_accounts = [
        ("wallet", identities["wallet"]["pubkey"], EVIDENCE / "wallet-account.json"),
        ("wsol-ata", identities["wsol_ata"]["pubkey"], EVIDENCE / "wsol-ata.json"),
        ("usdc-ata", identities["usdc_ata"]["pubkey"], EVIDENCE / "usdc-ata.json"),
    ]
    for _, _, path in genesis_accounts:
        if not path.is_file():
            raise SystemExit(f"Missing synthetic genesis account: {path}")
    rpc_url = f"http://127.0.0.1:{args.port}"
    with socket.socket() as probe:
        if probe.connect_ex(("127.0.0.1", args.port)) == 0:
            raise SystemExit(f"Loopback port {args.port} is already occupied; choose another port.")

    ledger = EVIDENCE / f"local-ledger-{args.port}"
    if ledger.exists() and not args.keep_ledger:
        shutil.rmtree(ledger)
    EVIDENCE.mkdir(parents=True, exist_ok=True)
    log_path = EVIDENCE / f"validator-{args.port}.log"
    cmd = [
        "solana-test-validator", "--ledger", str(ledger), "--url", MAINNET,
        "--rpc-port", str(args.port), "--faucet-port", str(args.port + 10000),
        "--gossip-port", str(args.port + 10001), "--dynamic-port-range",
        f"{args.port + 10002}-{args.port + 10102}",
        "--clone-upgradeable-program", *PROGRAMS,
        "--clone", *local_clone_addresses(manifest), MEMO, ATA,
    ]
    for _, address, path in genesis_accounts:
        cmd.extend(["--account", address, str(path)])
    # Capture the exact command, without credentials, plus public slot and
    # synthetic identity bindings in ignored evidence.
    (EVIDENCE / f"validator-{args.port}-command.json").write_text(json.dumps({
        "command": cmd, "public_rpc_read_only": MAINNET,
        "local_submission_rpc": rpc_url, "manifest_slot": manifest.get("snapshot_slot"),
        "synthetic_wallet": identities["wallet"]["pubkey"],
    }, indent=2) + "\n")
    with log_path.open("w") as log:
        validator = subprocess.Popen(cmd, cwd=ROOT, stdout=log, stderr=subprocess.STDOUT)
        try:
            deadline = time.monotonic() + 180
            while time.monotonic() < deadline:
                if validator.poll() is not None:
                    raise RuntimeError(f"validator exited with {validator.returncode}; see {log_path}")
                try:
                    if rpc(rpc_url, "getHealth", []) == "ok":
                        break
                except Exception:
                    time.sleep(1)
            else:
                raise RuntimeError(f"validator did not become healthy; see {log_path}")
            hashes = {"rpc": rpc_url, "genesis_hash": rpc(rpc_url, "getGenesisHash", []), "programs": {}}
            for program in PROGRAMS[:4]:
                account = rpc(rpc_url, "getAccountInfo", [program, {"encoding": "base64"}])["value"]
                if not account or not account.get("executable"):
                    raise RuntimeError(f"Cloned program missing or non-executable: {program}")
                program_bytes = base64.b64decode(account["data"][0])
                if account["owner"] != "BPFLoaderUpgradeab1e11111111111111111111111":
                    raise RuntimeError(f"Cloned program is not upgradeable-loader owned: {program}")
                if len(program_bytes) != 36 or int.from_bytes(program_bytes[:4], "little") != 2:
                    raise RuntimeError(f"Unexpected upgradeable Program account layout: {program}")
                programdata_address = base58_encode(program_bytes[4:36])
                programdata = rpc(rpc_url, "getAccountInfo", [programdata_address, {"encoding": "base64"}])["value"]
                if not programdata or programdata.get("owner") != account["owner"]:
                    raise RuntimeError(f"Cloned ProgramData missing or has unexpected owner: {program}")
                programdata_bytes = base64.b64decode(programdata["data"][0])
                if len(programdata_bytes) < 13 or int.from_bytes(programdata_bytes[:4], "little") != 3:
                    raise RuntimeError(f"Unexpected upgradeable ProgramData layout: {program}")
                authority_tag = programdata_bytes[12]
                # UpgradeableLoaderState::size_of_programdata_metadata() is
                # fixed at 45 bytes even when the optional upgrade authority
                # is None; the ELF always starts after that fixed metadata.
                code_offset = 45 if authority_tag in (0, 1) else None
                if code_offset is None or len(programdata_bytes) <= code_offset:
                    raise RuntimeError(f"Unexpected ProgramData authority/code layout: {program}")
                hashes["programs"][program] = {
                    "programdata_address": programdata_address,
                    "account_owner": account["owner"],
                    "program_account_sha256": hashlib.sha256(program_bytes).hexdigest(),
                    "programdata_account_sha256": hashlib.sha256(programdata_bytes).hexdigest(),
                    "deployed_elf_sha256": hashlib.sha256(programdata_bytes[code_offset:]).hexdigest(),
                    "deployment_slot": int.from_bytes(programdata_bytes[4:12], "little"),
                }
            for label, _, path in genesis_accounts:
                hashes.setdefault("synthetic_genesis_accounts", {})[label] = sha256(path)
            (EVIDENCE / f"validator-{args.port}-hashes.json").write_text(json.dumps(hashes, indent=2) + "\n")
            env = os.environ.copy()
            env.update({
                "GOFIN_METEORA_VALIDATOR_RPC": rpc_url,
                "GOFIN_METEORA_VALIDATOR_WALLET_KEY": identities["wallet"]["path"],
                "GOFIN_METEORA_VALIDATOR_POSITION_KEY": identities["position-mint"]["path"],
                "GOFIN_METEORA_VALIDATOR_WSOL_KEY": identities["temporary-wsol"]["path"],
                "GOFIN_METEORA_VALIDATOR_EXECUTION_EVIDENCE": str(EVIDENCE / f"validator-{args.port}-executions.json"),
            })
            execution_evidence = Path(env["GOFIN_METEORA_VALIDATOR_EXECUTION_EVIDENCE"])
            execution_evidence.unlink(missing_ok=True)
            command = ["cargo", "test", "--locked", "-p", "gofin-meteora", "--test", "validator"]
            if args.family == "dlmm":
                command.append("dlmm_native_swaps_and_full_position_lifecycle_on_local_validator")
            elif args.family == "damm":
                command.append("damm_v2_native_swap_and_full_position_lifecycle_on_local_validator")
            command.extend(["--", "--ignored", "--test-threads=1"])
            test = subprocess.run(command, cwd=ROOT, env=env, check=False)
            return test.returncode
        finally:
            validator.terminate()
            try:
                validator.wait(timeout=20)
            except subprocess.TimeoutExpired:
                validator.kill()
                validator.wait()


if __name__ == "__main__":
    sys.exit(main())
