#!/usr/bin/env python3
"""Discover Kamino dependencies through an owned Surfpool instance.

Surfpool owns public account fetching and loading. This module only interprets
protocol state to find the dependency graph, and applies the documented
local policy adjustments after execution Surfpool has cloned its own accounts.
No protocol account snapshot is retained or transferred between environments.
"""
from __future__ import annotations

import base64
import importlib.util
import json
import subprocess
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
ZERO = "11111111111111111111111111111111"
TOKEN = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"
KLEND = "KLend2g3cP87fffoy8q1mQqGKjrxjC8boSyAYavgmjD"
FARMS = "FarmsPZpWu9i7Kky8tPN37rs2TpmMrAZrC7S7vJa91Hr"
KVAULT = "KvauGMspG5k6rtzrqqn7WNn3oZdyKqLKwK2XWQ8FLjd"
LIQUIDITY = "6LtLpnUFNByNXLyCoK9wA2MykKAmQNZKBdY8s47dehDc"
ORCA = "whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc"
VAULT = "HDsayqAsDWy3QvANGqh2yNraqcD8Fnjgh73Mhb3WRS5E"
STRATEGY = "5BUR22MZR9YivcYpsx9xp2aDJGKBcF9HWCYynBPw6r8"
RESERVES = ["D6q6wuQSrifJKZYpR1M8R4YawnLDtDsMmWM1NbBmgJ59",
            "H3t6qZ1JkguCNTi9uzVKqQ7dvt2cum4XiXWom6Gn5e5S"]
STABLE_MINTS = ["EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
                "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB"]

# Import only the existing pure IDL decoding and address derivation helpers.
# Never call this legacy script's RPC, prepare, funding or account-file paths.
_SPEC = importlib.util.spec_from_file_location(
    "kamino_layout_helpers", ROOT / "scripts/verify-kamino-liquidity-validator.py")
_LAYOUT = importlib.util.module_from_spec(_SPEC)
_SPEC.loader.exec_module(_LAYOUT)


class Graph:
    def __init__(self, rpc):
        self.rpc = rpc
        self.accounts = {}

    def fetch(self, addresses, *, required=True):
        missing = [a for a in dict.fromkeys(addresses)
                   if a and a != ZERO and a not in self.accounts]
        for start in range(0, len(missing), 100):
            page = missing[start:start + 100]
            response = self.rpc("getMultipleAccounts", [page, {
                "encoding": "base64", "commitment": "finalized"}])
            if len(response["value"]) != len(page):
                raise RuntimeError("Surfpool returned an incomplete account response")
            for address, account in zip(page, response["value"]):
                if account is not None:
                    self.accounts[address] = account
                elif required:
                    raise RuntimeError(f"Required Kamino dependency is missing: {address}")

    def raw(self, address, *, owner=None, size=None):
        self.fetch([address])
        account = self.accounts[address]
        data = base64.b64decode(account["data"][0], validate=True)
        if owner is not None and account["owner"] != owner:
            raise RuntimeError(f"Unexpected owner for Kamino dependency {address}")
        if size is not None and len(data) != size:
            raise RuntimeError(f"Unexpected layout for Kamino dependency {address}")
        return data

    def farm(self, address):
        if address != ZERO:
            data = self.raw(address, owner=FARMS, size=8336)
            self.fetch([pub(data, offset) for offset in (40, 7256, 7536)])

    def asset(self, mint, decimals=None, token_program=TOKEN):
        data = self.raw(mint, owner=token_program)
        if len(data) < 82 or data[45] != 1:
            raise RuntimeError(f"Invalid initialized token mint: {mint}")
        if decimals is not None and data[44] != decimals:
            raise RuntimeError(f"Token decimals disagree with protocol state: {mint}")
        return {"mint": mint, "decimals": data[44], "token_program": token_program}


def pub(data, offset):
    if offset + 32 > len(data):
        raise RuntimeError("Public key offset exceeds account layout")
    return _LAYOUT.b58(data[offset:offset + 32])


def reserve_dependencies(graph, workdir):
    """Use the typed Rust layout; the transient decoder input is always removed."""
    graph.fetch(RESERVES)
    workdir.mkdir(parents=True, exist_ok=True)
    with tempfile.NamedTemporaryFile(mode="w", suffix=".json", prefix="reserves-",
                                     dir=workdir, delete=False) as handle:
        path = Path(handle.name)
        json.dump([{"address": address, "account": graph.accounts[address]}
                   for address in RESERVES], handle)
    try:
        return json.loads(subprocess.check_output([
            "cargo", "run", "--locked", "--offline", "--quiet", "-p", "gofin-kamino",
            "--example", "klend_snapshot_dependencies", "--", str(path)],
            cwd=ROOT, text=True))
    finally:
        path.unlink()


def borrowing(graph, workdir):
    dependencies = reserve_dependencies(graph, workdir)
    reserves = dependencies["reserves"]
    if [(r["mint"], r["decimals"]) for r in reserves] != [(m, 6) for m in STABLE_MINTS]:
        raise RuntimeError("Selected stablecoin reserve identity changed")
    if len({r["market"] for r in reserves}) != 1:
        raise RuntimeError("Selected reserves no longer share one lending market")
    graph.fetch(dependencies["dependencies"])
    for reserve in reserves:
        graph.raw(reserve["address"], owner=KLEND, size=8624)
        graph.farm(reserve["farm_collateral"])
        graph.farm(reserve["farm_debt"])
    return {"market": reserves[0]["market"], "reserves": reserves,
            "assets": [graph.asset(r["mint"], r["decimals"]) for r in reserves],
            "oracle_age_offsets": dependencies["oracle_age_offsets"],
            "market_policy_layout": dependencies["market_policy_layout"],
            "synthetic_oracle_max_age_seconds": 86400,
            "synthetic_market_borrow_disabled": 0}


def lending(graph):
    data = graph.raw(VAULT, owner=KVAULT, size=62552)
    mint, shares = pub(data, 80), pub(data, 184)
    farm = pub(data, 58600)
    graph.fetch([mint, shares, pub(data, 120), pub(data, 58568), farm,
                 _LAYOUT.derive(KVAULT, "string:global_config")])
    reserves = [pub(data, 312 + i * 2160) for i in range(25)
                if pub(data, 312 + i * 2160) != ZERO]
    ctokens = [pub(data, 344 + i * 2160) for i in range(25)
               if pub(data, 312 + i * 2160) != ZERO]
    graph.fetch(reserves + ctokens)
    for address in reserves:
        reserve = graph.raw(address, owner=KLEND, size=8624)
        graph.fetch([pub(reserve, offset) for offset in (32, 128, 160, 192, 2560, 2600)])
    graph.farm(farm)
    asset = graph.asset(mint)
    share_asset = graph.asset(shares)
    return {"vault": VAULT, "mint": mint, "decimals": asset["decimals"],
            "shares": shares, "share_asset": share_asset, "farm": farm,
            "reserves": reserves, "assets": [asset]}


def liquidity(graph):
    idl = _LAYOUT.load_idl("liquidity")
    state = _LAYOUT.decode(idl, "WhirlpoolStrategy", graph.raw(STRATEGY, owner=LIQUIDITY))
    if state["strategyDex"] != 0:
        raise RuntimeError("Selected liquidity strategy is no longer an Orca strategy")
    config_address = state["globalConfig"]
    config = _LAYOUT.decode(idl, "GlobalConfig", graph.raw(config_address, owner=LIQUIDITY))
    collateral = _LAYOUT.decode(idl, "CollateralInfos",
                               graph.raw(config["tokenInfos"], owner=LIQUIDITY))["infos"]
    # Authority PDAs need not have accounts. Keep discovered existing PDAs while
    # requiring all protocol state, vaults, token mints and configured rewards.
    required = [state[key] for key in (
        "tokenAVault", "tokenBVault", "pool", "position", "positionMint",
        "positionTokenAccount", "poolTokenVaultA", "poolTokenVaultB", "tokenAMint",
        "tokenBMint", "sharesMint", "tickArrayLower", "tickArrayUpper",
        "strategyLookupTable", "farm", "reward0Vault", "reward1Vault", "reward2Vault")]
    authority = state["baseVaultAuthority"]
    required = [key for key in required if key != authority]
    required += [config["tokenInfos"], config["scopeProgramId"]]
    required += [collateral[state[key]]["scopeFeed"]
                 for key in ("tokenACollateralId", "tokenBCollateralId")]
    for i in range(3):
        if state[f"reward{i}Vault"] not in (ZERO, authority):
            required.append(collateral[state[f"reward{i}CollateralId"]]["scopeFeed"])
    for reward in state["kaminoRewards"]:
        if reward["rewardMint"] != ZERO and reward["decimals"] > 0:
            required += [reward["rewardMint"], collateral[reward["rewardCollateralId"]]["scopeFeed"]]
    graph.fetch(required)
    optional = [state[key] for key in ("baseVaultAuthority", "sharesMintAuthority",
        "raydiumProtocolPositionOrBaseVaultAuthority", "raydiumPoolConfigOrBaseVaultAuthority")]
    optional += [_LAYOUT.derive(LIQUIDITY, "string:treasury_fee_vault", f"pubkey:{mint}")
                 for mint in (state["tokenAMint"], state["tokenBMint"])]
    optional.append(_LAYOUT.derive(LIQUIDITY, "string:treasury_fee_vault_authority"))
    graph.fetch(optional, required=False)
    pool = _LAYOUT.decode(idl, "Whirlpool", graph.raw(state["pool"], owner=ORCA))
    graph.fetch([r.get("tokenVault", r.get("vault", ZERO)) for r in pool["rewardInfos"]]
                + [r.get("tokenMint", r.get("mint", ZERO)) for r in pool["rewardInfos"]])
    graph.farm(state["farm"])
    assets = [graph.asset(state[f"token{side}Mint"], state[f"token{side}MintDecimals"],
                         state[f"token{side}TokenProgram"] if state[f"token{side}TokenProgram"] != ZERO else TOKEN)
              for side in ("A", "B")]
    shares = state["sharesMint"]
    return {"strategies": [{"address": STRATEGY, "backend": "orca", "assets": assets}],
            "assets": assets, "shares": shares, "share_asset": graph.asset(shares),
            "global_config": config_address, "deposit_units": 1, "stake": False,
            "synthetic_policy": {"maxPriceDeviationBps": 10000,
                                 "depositMaxPoolPriceOracleDeviationBps": 10000}}


def discover(family, rpc, workdir: Path) -> dict:
    """Return identities and addresses, never account bytes, from local Surfpool."""
    graph = Graph(rpc)
    if family == "borrowing":
        manifest = borrowing(graph, workdir)
    elif family == "lending":
        manifest = lending(graph)
    elif family == "liquidity":
        manifest = liquidity(graph)
    else:
        raise ValueError(f"Unknown Kamino fixture family: {family}")
    manifest["accounts"] = sorted(graph.accounts)
    return manifest


def set_local_data(rpc, address, data, owner):
    rpc("surfnet_setAccount", [address, {"data": data.hex()}])
    if Graph(rpc).raw(address, owner=owner) != data:
        raise RuntimeError(f"Surfpool did not retain the requested local policy: {address}")


def apply_policies(family, rpc, manifest) -> None:
    """Patch only the allowed local policy fields, using execution-state bytes."""
    graph = Graph(rpc)
    if family == "borrowing":
        # Rediscover typed offsets from the final execution environment rather
        # than trusting offsets or account data from the discovery process.
        with tempfile.TemporaryDirectory(prefix="gofin-kamino-layout-") as directory:
            current = reserve_dependencies(graph, Path(directory))
        if any(current[key] != manifest[key] for key in (
                "reserves", "oracle_age_offsets", "market_policy_layout")):
            raise RuntimeError("Borrowing identity/layout changed before policy application")
        # A public market pause must not disable this synthetic positive
        # lifecycle. Change only the cloned market's borrowing switch; all
        # production market/obligation checks and risk limits remain active.
        layout = current["market_policy_layout"]
        market = bytearray(graph.raw(manifest["market"], owner=KLEND,
                                     size=layout["account_size"]))
        offset = layout["borrow_disabled_offset"]
        if offset < 8 or offset >= len(market):
            raise RuntimeError("Borrowing policy offset exceeds market layout")
        manifest["market_borrowing_policy"] = {
            "environment": "synthetic_surfpool",
            "market": manifest["market"],
            "cloned_borrow_disabled": market[offset],
            "local_borrow_disabled": 0,
        }
        market[offset] = 0
        set_local_data(rpc, manifest["market"], market, KLEND)
        for reserve in current["reserves"]:
            data = bytearray(graph.raw(reserve["address"], owner=KLEND, size=8624))
            for offset in current["oracle_age_offsets"]:
                if offset < 8 or offset + 8 > len(data):
                    raise RuntimeError("Oracle age offset exceeds reserve layout")
                data[offset:offset + 8] = (86400).to_bytes(8, "little")
            set_local_data(rpc, reserve["address"], data, KLEND)
    elif family == "liquidity":
        idl = _LAYOUT.load_idl("liquidity")
        for address, name, fields in (
            (STRATEGY, "WhirlpoolStrategy", {"maxPriceDeviationBps": 10000,
                                           "depositMaxPoolPriceOracleDeviationBps": 10000}),
            (manifest["global_config"], "GlobalConfig", {"depositMaxPoolPriceOracleDeviationBps": 10000}),
        ):
            data = graph.raw(address, owner=LIQUIDITY)
            state = _LAYOUT.decode(idl, name, data)
            if name == "WhirlpoolStrategy" and (state["strategyDex"] != 0 or state["globalConfig"] != manifest["global_config"]):
                raise RuntimeError("Liquidity identity changed before policy application")
            patched = _LAYOUT.patch_u64(idl, name, data, fields)
            if any(_LAYOUT.decode(idl, name, patched)[field] != value for field, value in fields.items()):
                raise RuntimeError("Local liquidity policy patch failed layout validation")
            set_local_data(rpc, address, patched, LIQUIDITY)
    elif family != "lending":
        raise ValueError(f"Unknown Kamino fixture family: {family}")
