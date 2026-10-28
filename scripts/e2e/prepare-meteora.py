#!/usr/bin/env python3
"""Discover Meteora dependencies through local Surfpool; never create fixtures."""
from __future__ import annotations
import importlib.util
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location("meteora_decoding", ROOT / "scripts/verify-meteora-validator.py")
codec = importlib.util.module_from_spec(spec)
spec.loader.exec_module(codec)
POOLS = {
    "dlmm": "5rCf1DM8LjKTw4YqhnoLcngyZYeNnQqztScTogYHAS6",
    "damm_v2": "8Pm2kZpnxD3hoMmt4bjStX2Pw2Z9abpbHzZxMPqxPmie",
}
WSOL = "So11111111111111111111111111111111111111112"
USDC = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"


def discover(family, rpc):
    """Retain layout/PDA discovery while Surfpool owns every upstream read."""
    accounts = {}

    def fetch(addresses, optional=False):
        rows = rpc("getMultipleAccounts", [addresses, {"encoding": "base64", "commitment": "finalized"}])["value"]
        if len(rows) != len(addresses):
            raise RuntimeError("Incomplete Meteora dependency response")
        for address, account in zip(addresses, rows):
            if account is None:
                if not optional:
                    raise RuntimeError(f"Missing Meteora dependency {address}")
            else:
                accounts[address] = account
        return rows

    pool = POOLS[family]
    program = codec.PROGRAMS[0 if family == "dlmm" else 1]
    account = fetch([pool])[0]
    data = codec.account_data(account, family)
    if account["owner"] != program or len(data) < (904 if family == "dlmm" else 1112):
        raise RuntimeError("Selected Meteora pool has an unsupported owner or layout")
    manifest = {"family": family, "program": program, "pool": pool}
    if family == "dlmm":
        active_bin = int.from_bytes(data[76:80], "little", signed=True)
        manifest.update(active_bin=active_bin, active_bin_array=active_bin // 70,
            mint_a=codec.account_pubkey(data, 88), mint_b=codec.account_pubkey(data, 120),
            vault_a=codec.account_pubkey(data, 152), vault_b=codec.account_pubkey(data, 184))
        oracle = codec.derive_address(program, "string:oracle", f"pubkey:{pool}")
        bitmap = codec.derive_address(program, "string:bitmap", f"pubkey:{pool}")
        fetch([oracle])
        fetch([bitmap], optional=True)
        arrays = {str(index): codec.derive_address(program, "string:bin_array", f"pubkey:{pool}", f"i64le:{index}")
                  for index in range(active_bin // 70 - 15, active_bin // 70 + 16)}
        rows = fetch(list(arrays.values()), optional=True)
        manifest["bin_arrays"] = {index: address for (index, address), row in zip(arrays.items(), rows) if row is not None}
        if not manifest["bin_arrays"]:
            raise RuntimeError("No initialized DLMM bin arrays around the active bin")
        if any(row is not None and row["owner"] != program for row in rows):
            raise RuntimeError("DLMM bin array has an unexpected owner")
    else:
        manifest.update(mint_a=codec.account_pubkey(data, 168), mint_b=codec.account_pubkey(data, 200),
                        vault_a=codec.account_pubkey(data, 232), vault_b=codec.account_pubkey(data, 264))
    if (manifest["mint_a"], manifest["mint_b"]) != (WSOL, USDC):
        raise RuntimeError("Selected Meteora pool no longer matches WSOL/USDC ordering")
    fetch([manifest["mint_a"], manifest["mint_b"], manifest["vault_a"], manifest["vault_b"]])
    assets = []
    for mint, expected_decimals in ((WSOL, 9), (USDC, 6)):
        account = accounts[mint]
        data = codec.account_data(account, "mint")
        if account["owner"] not in codec.PROGRAMS[2:] or len(data) < 82 or data[45] != 1 or data[44] != expected_decimals:
            raise RuntimeError("Meteora mint owner, initialization or decimals changed")
        assets.append({"mint": mint, "token_program": account["owner"], "decimals": expected_decimals})
    return {**manifest, "accounts": sorted(accounts), "assets": assets}
