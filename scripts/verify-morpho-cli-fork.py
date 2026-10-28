#!/usr/bin/env python3
"""Morpho V2 public CLI named-plan lifecycle on an independently owned fork.

The immutable deployed vault/bundle/distributor contracts execute normally.
Fixture funding, Merkl tree data and exact USD prices are synthetic, confined
to the guarded owned Anvil endpoint. All submissions use make dev-env-cli.
"""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import tempfile
import time

from gofin_dev_resources import allocate_loopback, start_owned_process
from aave_fork_receipt_proxy import ReceiptProxy

SOURCE = Path(__file__).resolve().parent.parent
SPEC = importlib.util.spec_from_file_location("morpho_amm_helpers", Path(__file__).with_name("verify-amm-cli-plan-fork.py"))
AMM = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(AMM)
VAULTS = {1: "0x04422053addbc9bb2759b248b574e3fca76bc145", 8453: "0x050ce30b927da55177a4914ec73480238bad56f0"}
BUNDLES = {1: "0x02912516d49de997db75b9d7858faae59209650b", 8453: "0x2b08a911f48de25a7e305d910afb5597abe8ea7b"}
WETH = {1: "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2", 8453: "0x4200000000000000000000000000000000000006"}
DISTRIBUTOR = "0x3ef3d8ba38ebe18db133cec108f4d14ce00dd9ae"
ARCHIVES = {1: "https://ethereum-rpc.publicnode.com", 8453: "https://base-rpc.publicnode.com"}
CAST = str(Path.home() / ".foundry/bin/cast")


def public_rpc(url, method, params):
    """Read-only public bootstrap; curl uses this host's working TLS proxy."""
    if method not in {"eth_chainId", "eth_getBlockByNumber", "eth_getCode", "eth_getStorageAt", "eth_call"}:
        raise RuntimeError("Public fork bootstrap forbids financial or mutable RPC methods")
    request = json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params})
    result = subprocess.run(["curl", "--max-time", "25", "-fsSL", url, "-H", "Content-Type: application/json", "--data", request], capture_output=True, text=True, timeout=30)
    if result.returncode:
        raise RuntimeError(f"Public archive bootstrap unavailable: {method}: {result.stderr[:200]}")
    value = json.loads(result.stdout)
    if "error" in value:
        raise RuntimeError(f"Public archive bootstrap rejected {method}: {value['error']}")
    return value["result"]


def cast(*args):
    result = subprocess.run([CAST, *args], capture_output=True, text=True, timeout=20)
    if result.returncode:
        raise RuntimeError(f"Fixture ABI encoding failed: {result.stderr[:200]}")
    return result.stdout.strip()


class MorphoRehearsal(AMM.Rehearsal):
    receipt_proxy = None
    rpc_raw = staticmethod(AMM.FORK.rpc)

    def __init__(self, evidence, chain, pin=None, rpc_url=None):
        super().__init__(evidence, f"morpho-{chain}")
        self.chain, self.saved_pin = chain, pin
        self.archive_url = rpc_url or ARCHIVES[chain]
        self.vault, self.token = VAULTS[chain], AMM.FORK.TOKENS[chain]
        self.base = {"wallet": "dev-wallet", "network": "ethereum", "chain_id": chain}

    def make(self, target, **variables):
        # Source snapshots are intentionally rejected if another contributor
        # edits during a build. Retry only setup, never a financial command.
        for attempt in range(3):
            try:
                return super().make(target, **variables)
            except RuntimeError as exc:
                if target not in {"dev-env-import", "dev-env-start"} or "SOURCE_CHANGED_DURING_BUILD" not in str(exc) or attempt == 2:
                    raise
                self.save("source-changed-build-retry", {"target": target, "attempt": attempt + 1, "reason": "Source changed during snapshot build; retain environment and require a new valid build."})

    def close(self):
        try:
            super().close()
        finally:
            if self.receipt_proxy:
                self.receipt_proxy.stop()

    def cli(self, *argv, error=None):
        header = self.rpc("eth_getBlockByNumber", ["latest", False], "Read owned fork timestamp before public CLI dispatch")
        now = int(time.time())
        if int(header["timestamp"], 16) + 30 < now:
            self.rpc("evm_setNextBlockTimestamp", [now], "Advance only the owned historical fork to current wall time")
            self.rpc("evm_mine", [], "Keep production RPC freshness validation enabled")
        return super().cli(*argv, error=error)

    def call(self, contract, signature, *args):
        return self.rpc("eth_call", [{"to": contract, "data": cast("calldata", signature, *map(str, args))}, "latest"], "Read deployed contract for exact fork fixture setup")

    def fund_token(self, contract, owner, amount):
        """Discover the balance mapping on this exact fork; restore failed probes."""
        before = int(self.call(contract, "balanceOf(address)", owner), 16)
        for slot in range(64):
            key = cast("index", "address", owner, str(slot))
            old = self.rpc("eth_getStorageAt", [contract, key, "latest"], "Save original owned-fork storage before fixture funding probe")
            self.rpc("anvil_setStorageAt", [contract, key, "0x" + format(amount, "064x")], "Synthetic ERC-20 fixture funding on the exact owned fork")
            observed = int(self.call(contract, "balanceOf(address)", owner), 16)
            if observed == amount:
                self.save("synthetic-funding", {"contract": contract, "owner": owner, "before": str(before), "after": str(amount), "balance_mapping_slot": slot, "production_writes": 0})
                return
            self.rpc("anvil_setStorageAt", [contract, key, old], "Restore unsuccessful synthetic balance probe exactly")
        raise RuntimeError("Could not identify the deployed ERC-20 balance mapping on the owned fork")

    def install_reward_fixture(self):
        # Reviewed Distributor storage follows Initializable and AccessControl
        # inherited state. Locate tree and lastTree by their public getters;
        # then verify getMerkleRoot resolves the synthetic leaf. No code changes.
        root = cast("keccak", cast("abi-encode", "f(address,address,uint256)", self.owner, self.token, "1000000"))
        current_tree = self.call(DISTRIBUTOR, "tree()")
        previous_tree = self.call(DISTRIBUTOR, "lastTree()")
        tree_root, last_root = current_tree[:66], previous_tree[:66]
        locations = {}
        for slot in [101, 103, *[n for n in range(256) if n not in (101, 103)]]:
            stored = self.rpc("eth_getStorageAt", [DISTRIBUTOR, hex(slot), "latest"], "Locate reviewed Merkl root slots on owned fork using exact public getter values")
            if stored == tree_root:
                locations.setdefault("tree", []).append(slot)
            if stored == last_root:
                locations.setdefault("lastTree", []).append(slot)
            if locations.get("tree") and locations.get("lastTree"):
                pairs = [(a, b) for a in locations["tree"] for b in locations["lastTree"] if b == a + 2]
                if len(pairs) == 1:
                    tree_slot, last_slot = pairs[0]
                    for target in (tree_slot, last_slot):
                        self.rpc("anvil_setStorageAt", [DISTRIBUTOR, hex(target), root], "Synthetic Merkl entitlement root, exclusively in the owned fork")
                    if self.call(DISTRIBUTOR, "getMerkleRoot()") == root:
                        self.save("merkl-synthetic-entitlement", {"root": root, "owner": self.owner, "token": self.token, "cumulative": "1000000", "pending": "50000", "tree_slot": tree_slot, "last_tree_slot": last_slot, "evidence": "Synthetic entitlement at deployed distributor; not live eligibility"})
                        return
                    raise RuntimeError("Synthetic Merkl root did not become active on owned fork")
        raise RuntimeError("Reviewed Merkl tree storage could not be located unambiguously")

    def start(self):
        chain, url = self.chain, self.archive_url
        assert int(public_rpc(url, "eth_chainId", []), 16) == chain
        if self.saved_pin:
            anchor = public_rpc(url, "eth_getBlockByNumber", [hex(self.saved_pin["block"]), False])
            assert anchor["hash"] == self.saved_pin["hash"], "Saved canonical fork block changed"
        else:
            head = public_rpc(url, "eth_getBlockByNumber", ["latest", False])
            anchor = public_rpc(url, "eth_getBlockByNumber", [hex(int(head["number"], 16) - (5 if chain == 1 else 10)), False])
        self.save("archive-anchor", {"chain_id": chain, "block": int(anchor["number"], 16), "hash": anchor["hash"], "rpc": url, "production_writes": 0})
        # Fail before any fixture mutation if the archive cannot serve deployed state.
        assert len(public_rpc(url, "eth_getCode", [self.vault, anchor["number"]])) > 2
        lease = allocate_loopback()
        self.endpoint, port = lease.url, lease.port
        lease.release()
        self.node = start_owned_process([str(Path.home() / ".foundry/bin/anvil"), "--host", "127.0.0.1", "--port", str(port), "--chain-id", str(chain), *(["--optimism"] if chain == 8453 else []), "--fork-url", url, "--fork-block-number", str(int(anchor["number"], 16)), "--timestamp", str(int(time.time())), "--base-fee", "100000000", "--gas-price", "10000000", "--disable-min-priority-fee", "--no-cors", "--block-time", "1"], log_path=self.directory / "anvil.log", marker_path=self.directory / "anvil.owner.json", env=self.env)
        for _ in range(100):
            if self.node.process.poll() is not None:
                break
            try:
                self.identity = AMM.FORK.rpc(self.endpoint, "anvil_metadata", [])
                break
            except (OSError, RuntimeError):
                time.sleep(.25)
        if not self.identity:
            raise RuntimeError("Owned Anvil could not load the public archive; inspect retained anvil.log")
        assert self.identity["chainId"] == chain
        assert self.identity["forkedNetwork"]["forkBlockHash"] == anchor["hash"]
        assert self.identity["forkedNetwork"]["forkBlockNumber"] == int(anchor["number"], 16)
        keyfile = self.directory / "PUBLIC-SYNTHETIC-NEVER-FUND.env"
        keyfile.write_text(f"GOFIN_DEV_EVM_PRIVATE_KEY={AMM.KEY}\n")
        keyfile.chmod(0o600)
        try:
            self.make("dev-env-import", DEV_CREDENTIALS_FILE=str(keyfile), DEV_FLAGS="--json")
        finally:
            keyfile.unlink()
        worktree_key = hashlib.sha256(str(SOURCE).encode()).hexdigest()[:20]
        self.config_path = Path(self.env["GOFIN_DEV_STATE_HOME"]) / worktree_key / self.name / "config.json"
        config = json.loads(self.config_path.read_text())
        assert config["environment_id"] == hashlib.sha256(f"{worktree_key}:{self.name}".encode()).hexdigest()
        self.owner = config["evm_addresses"][0]
        self.receipt_proxy = ReceiptProxy(self, self.endpoint, chain, WETH[chain])
        self.endpoint = self.receipt_proxy.start()
        config["rpc_urls"], config["reviewed_chain_ids"] = {str(chain): [self.endpoint]}, [chain]
        config["morpho_fork_rewards"] = True
        self.config_path.write_text(json.dumps(config, indent=2) + "\n")
        self.config_path.chmod(0o600)
        self.rpc("anvil_setBalance", [self.owner, hex(10**17)], "Synthetic 0.1 ETH owned-fork native funding")
        self.fund_token(self.token, self.owner, 10_000_000)
        if int(self.call(self.token, "balanceOf(address)", DISTRIBUTOR), 16) < 1_000_000:
            self.fund_token(self.token, DISTRIBUTOR, 1_000_000)
        self.install_reward_fixture()
        self.started = True
        self.make("dev-env-start", DEV_MODE="live", DEV_FLAGS="--json")
        for operation in ["morpho.vaults", "morpho.vault", "morpho.positions", "morpho.deposit", "morpho.withdraw", "morpho.redeem", "morpho.rewards", "morpho.rewards.claim", "plans.authorize"]:
            self.cli("schema", operation)

    def state(self):
        return {"balances": self.read("wallet.balances", {**self.base, "tokens": [self.token]}), "positions": self.read("morpho.positions", {**self.base, "vault": self.vault}), "allowances": {token: self.read("wallet.allowance", {**self.base, "token": token, "spender": BUNDLES[self.chain]}) for token in (self.token, self.vault)}}

    def plan(self, state):
        resource = self.initial["balances"]["resource"]
        share_decimals = state["shares"]["decimals"]
        quote = int(self.call(self.vault, "previewDeposit(uint256)", 1_000_000), 16)
        minimum = AMM.decimal(quote * 99 // 100, share_decimals)
        maximum = AMM.decimal(quote * 2, share_decimals)

        def asset(token):
            return {"network": f"ethereum:{self.chain}", "reference": token, "decimals": 6 if token == self.token else 18}

        def cost(atoms):
            return {"location": {"kind": "cash", "resource": resource}, "amount": {"asset": asset("native"), "atoms": str(atoms)}, "treatment": "additional_debit"}

        steps = []

        def add(name, operation, extra, bounds):
            steps.append({"step_id": name, "operation": operation, "input": {**self.base, "request_id": self.name + "-" + name, "max_fee_eth": "0.001", **extra}, "permitted_variations": bounds, "completion": {"operation_succeeded": True}, "funding_dependencies": []})

        def bounds(lo, hi, **extra):
            return {"min_amount": lo, "max_amount": hi, "max_fee_eth": "0.001", **extra}

        add("approve-asset", "wallet.approve", {"token": self.token, "spender": BUNDLES[self.chain], "amount": "1"}, bounds("1", "1", vault=self.vault))
        add("deposit", "morpho.deposit", {"vault": self.vault, "amount": "1", "min_shares": minimum}, bounds("1", "1"))
        add("approve-shares", "wallet.approve", {"token": self.vault, "spender": BUNDLES[self.chain], "amount": maximum}, bounds(maximum, maximum, vault=self.vault))
        add("partial-withdraw", "morpho.withdraw", {"vault": self.vault, "amount": "0.4", "max_shares": maximum}, bounds("0.4", "0.4"))
        add("full-redeem", "morpho.redeem", {"vault": self.vault, "amount": "all", "max_shares": maximum, "accept_prevailing_value": True}, bounds("0", maximum, max_receipt_assets="2"))
        add("revoke-shares", "wallet.approve", {"token": self.vault, "spender": BUNDLES[self.chain], "amount": "0"}, bounds("0", "0", vault=self.vault))
        add("claim-rewards", "morpho.rewards.claim", {"tokens": [self.token]}, {"max_amounts": {self.token: "1"}, "max_fee_eth": "0.001"})
        return {"version": 1, "name": "Morpho V2 owned-fork lifecycle with synthetic Merkl entitlement", "steps": steps, "aggregate_bounds": {"maximum_debits": [{"resource": resource, "amount": {"asset": asset(self.token), "atoms": "1000000"}}], "maximum_costs": [cost(7 * 10**15)]}}

    def run(self, positive_only=False):
        self.read("morpho.vaults", {"network": "ethereum", "chain_id": self.chain, "asset": self.token, "limit": 20})
        vault = self.read("morpho.vault", {**self.base, "vault": self.vault})
        if vault.get("state") is None:
            raise RuntimeError(f"Owned-fork deployed vault observation unavailable: {vault.get('write_compatibility', vault)}")
        assert vault["version"] == 2 and vault["state"]["asset"]["reference"] == self.token
        self.initial = self.state()
        assert int(self.initial["positions"]["positions"][0]["shares"]["atoms"]) == 0
        rewards = self.read("morpho.rewards", {**self.base, "tokens": [self.token]})
        assert rewards["rewards"][0]["claimable"] == "1000000"
        plan = self.plan(vault["state"])
        review = self.cli("plans", "preview", "--input", json.dumps(plan), "--valid-for", "30m")
        grant = self.cli("plans", "authorize", "--input", json.dumps(plan), "--id", self.name + "-grant", "--review-hash", review["review_hash"], "--valid-for", "30m")
        pid = grant["plan_id"]
        start_submissions = self.submissions()
        if not positive_only:
            self.cli("execute", plan["steps"][1]["operation"], "--input", json.dumps(plan["steps"][1]["input"]), "--id", self.name + "-out-of-order", "--plan", pid, "--step", "deposit", "--dry-run", error=True)
            assert self.submissions() == start_submissions
        records = []
        for step in plan["steps"]:
            record = self.settled(step["operation"], step["input"], pid, step["step_id"])
            records.append(record)
            if step["step_id"] == "deposit":
                held = self.read("morpho.positions", {**self.base, "vault": self.vault})["positions"][0]
                assert int(held["shares"]["atoms"]) > 0 and int(held["underlying"]["atoms"]) > 0
                self.restart()
                recovered = self.cli("status", record["id"])
                assert recovered["submission"] == record["submission"] and recovered["effects"] == record["effects"]
        final = self.state()
        assert final["positions"]["positions"][0]["shares"]["atoms"] == "0"
        assert all(row["data"]["units"] == "0" for row in final["allowances"].values())
        final_rewards = self.read("morpho.rewards", {**self.base, "tokens": [self.token]})["rewards"][0]
        assert final_rewards["claimed"] == "1000000" and final_rewards["claimable"] == "0" and final_rewards["pending"] == "50000"
        def cash(snapshot):
            return {row["amount"]["asset"]["reference"]: int(row["amount"]["atoms"]) for row in snapshot["balances"]["data"]}
        initial_cash, final_cash = cash(self.initial), cash(final)
        effects = [effect for record in records for effect in record["effects"]["effects"]]
        fees = sum(int(effect["amount"]["atoms"]) for effect in effects if effect["kind"] == "cost" and effect["amount"]["asset"]["reference"] == "native")
        credits = sum(int(effect["amount"]["atoms"]) for effect in effects if effect["kind"] == "usable_credit" and effect["amount"]["asset"]["reference"] == self.token)
        debits = sum(int(effect["amount"]["atoms"]) for effect in effects if effect["kind"] == "cash_debit" and effect["amount"]["asset"]["reference"] == self.token)
        assert debits == 1_000_000 and credits >= 1_400_000
        assert final_cash[self.token] == initial_cash[self.token] - debits + credits
        assert final_cash["native"] == initial_cash["native"] - fees
        status = self.cli("plans", "status", pid)
        assert all(step["state"] == "completed" for step in status["steps"]) and not status["usage"]["reservations"]
        assert self.submissions() == start_submissions + 7
        self.save("result", {"chain_id": self.chain, "status": "passed", "approval": "automatic isolated development approval", "authorization_mode": "single named plan", "production_writes": 0, "financial_submissions": 7, "position_closed": True, "allowances_zero": True, "cash_conservation_exact": True, "native_fees_atoms": str(fees), "duplicate_submissions_preserved": True, "restart_with_position": True, "merkl_evidence": "positive synthetic entitlement against deployed immutable distributor implementation on owned fork", "baseline": self.initial, "final": final, "plan_status": status, "native_approval": "not run", "valuation_evidence": "synthetic fixed USD prices guarded by exact owned fork identity; not live pricing", "base_fee_evidence": "local modeled receipt components, not authentic rollup fee evidence" if self.chain == 8453 else "canonical fork receipt"})


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--chain", required=True, type=int, choices=(1, 8453))
    parser.add_argument("--pin", type=Path, help="Reuse an earlier exact public block/hash")
    parser.add_argument("--rpc-url", help="Archive RPC override; chain identity and pinned block/hash checks remain required")
    parser.add_argument("--positive-only", action="store_true", help="Run the successful lifecycle without the out-of-order rejection case")
    args = parser.parse_args()
    AMM.CHAIN, AMM.TOKEN = args.chain, AMM.FORK.TOKENS[args.chain]
    AMM.BASE = {"wallet": "dev-wallet", "network": "ethereum", "chain_id": args.chain}
    root = SOURCE / ".context" / "test-runs"
    root.mkdir(parents=True, exist_ok=True, mode=0o700)
    evidence = Path(tempfile.mkdtemp(prefix=f"morpho-cli-fork-{args.chain}.", dir=root))
    print(json.dumps({"evidence": str(evidence), "production_writes": 0}), flush=True)
    rehearsal = MorphoRehearsal(evidence, args.chain, json.loads(args.pin.read_text()) if args.pin else None, args.rpc_url)
    try:
        rehearsal.start()
        rehearsal.run(positive_only=args.positive_only)
        print(json.dumps({"chain_id": args.chain, "result": "passed", "evidence": str(evidence)}), flush=True)
        return 0
    except Exception as error:
        rehearsal.save("failure", {"chain_id": args.chain, "error": str(error), "production_writes": 0})
        raise
    finally:
        rehearsal.close()


if __name__ == "__main__":
    raise SystemExit(main())
