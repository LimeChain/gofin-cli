#!/usr/bin/env python3
"""Public CLI rehearsal: 2 USDC -> ETH -> USDC, one grant on an owned fork.

Default funding must block the full-proceeds return; principal_only must pass.
Only public synthetic credentials are generated. No existing GoFin state is read.
Public pricing uses the normal adapter; it is not synthetic valuation evidence.
"""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shlex
import subprocess
import tempfile
import time
import uuid
from gofin_dev_resources import allocate_loopback, start_owned_process

SOURCE = Path(__file__).resolve().parent.parent
SPEC = importlib.util.spec_from_file_location("amm_fork", Path(__file__).with_name("verify-amm-fork.py"))
FORK = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(FORK)
# Public SHA256 fixture; NEVER FUND outside this owned disposable fork.
KEY = "476361fd04a5ecc99b558a630122e8dd897f82766ddf5a4c59a8a5230d4f91bf"
CHAIN = 42161
TOKEN = FORK.TOKENS[CHAIN]  # explicit test fixture, never a product default
FEE_ATOMS = 10**16
BASE = {"wallet": "dev-wallet", "network": "evm", "chain_id": CHAIN}


def decimal(atoms, decimals):
    whole, part = divmod(int(atoms), 10**decimals)
    return f"{whole}.{part:0{decimals}d}".rstrip("0").rstrip(".") if part else str(whole)


def conversion(record):
    return next(effect for effect in record["effects"]["effects"] if effect["kind"] == "conversion")


class Rehearsal:
    def __init__(self, evidence, kind, return_dex="uniswap", return_fee=100, yolo=False):
        self.directory = evidence / kind
        self.directory.mkdir()
        self.name = "cli-plan-" + kind
        self.return_dex, self.return_fee = return_dex, return_fee
        self.env = {**os.environ, "GOFIN_DEV_STATE_HOME": str(self.directory / "state")}
        self.sequence = 0
        self.node = None
        self.endpoint = None
        self.identity = None
        self.started = False
        self.config_path = None
        self.yolo = yolo

    def save(self, label, value):
        self.sequence += 1
        (self.directory / f"{self.sequence:03d}-{label}.json").write_text(json.dumps(value, indent=2) + "\n")

    def make(self, target, **variables):
        result = subprocess.run(["make", "--no-print-directory", target, f"DEV_ENV={self.name}", *[f"{key}={value}" for key, value in variables.items()]], cwd=SOURCE, env=self.env, capture_output=True, text=True, timeout=600)
        if result.returncode:
            detail = result.stderr[-1500:]
            try:
                failure = json.loads(result.stdout).get("error", {})
                detail = f"{failure.get('code', 'UNKNOWN')}: {failure.get('message', '')}; {detail}"
            except (ValueError, AttributeError):
                pass
            raise RuntimeError(f"{target} failed: {detail}")
        return result

    def guard(self):
        assert self.endpoint and self.endpoint.startswith("http://127.0.0.1:")
        assert self.node and self.node.process.poll() is None
        config = json.loads(self.config_path.read_text())
        assert config["rpc_urls"] == {str(CHAIN): [self.endpoint]}
        assert config["reviewed_chain_ids"] == [CHAIN]
        metadata = FORK.rpc(self.endpoint, "anvil_metadata", [])
        assert metadata["instanceId"] == self.identity["instanceId"]
        assert metadata["forkedNetwork"] == self.identity["forkedNetwork"]
        self.save("ownership-guard", {"method": "anvil_metadata", "params": [],
                  "reason": "Verify exact owned local fork before public CLI dispatch", "production_writes": 0})

    def cli(self, *argv, error=None):
        self.guard()
        # Make forwards this quoted argv to its already-snapshotted CLI.
        result = subprocess.run(["make", "--no-print-directory", "dev-env-cli", f"DEV_ENV={self.name}", "ARGS=" + shlex.join([*argv, "--json"])], cwd=SOURCE, env=self.env, capture_output=True, text=True, timeout=180)
        try:
            value = json.loads(result.stdout)
        except ValueError as exc:
            raise RuntimeError(f"CLI did not emit one JSON value: {result.stderr[-1000:]}") from exc
        self.save("cli", {"argv": argv, "exit": result.returncode, "result": value})
        if error:
            assert result.returncode != 0 and value.get("ok") is False, value
            if error is not True:
                assert value["error"]["code"] == error, value
        else:
            assert result.returncode == 0, value
        return value

    def rpc(self, method, params, reason):
        assert self.endpoint.startswith("http://127.0.0.1:")
        metadata = FORK.rpc(self.endpoint, "anvil_metadata", [])
        if self.identity:
            assert metadata["instanceId"] == self.identity["instanceId"]
            assert metadata["forkedNetwork"] == self.identity["forkedNetwork"]
        self.save("local-rpc", {"method": method, "params": params, "reason": reason,
                               "guard": "anvil_metadata exact owned instance/fork anchor", "production_writes": 0})
        return FORK.rpc(self.endpoint, method, params)

    def read(self, operation, payload):
        return self.cli("read", operation, "--input", json.dumps(payload))

    def snapshot(self):
        balances = self.read("wallet.balances", {**BASE, "tokens": [TOKEN]})
        token = self.read("wallet.token", {**BASE, "token": TOKEN})
        allowances = {dex: self.read("wallet.allowance", {**BASE, "token": TOKEN, "spender": FORK.protocol_config(CHAIN, dex)["router"]}) for dex in ("pancakeswap", self.return_dex)}
        return {"balances": balances, "token": token, "allowances": allowances}

    def operations(self):
        value = self.cli("operations")
        return value if isinstance(value, list) else value["operations"]

    def submissions(self):
        return sum(record.get("submission") is not None for record in self.operations())

    def restart(self):
        self.make("dev-env-stop", DEV_FLAGS="--json")
        self.started = False
        self.started = True
        self.make("dev-env-start", DEV_MODE="live", DEV_FLAGS="--json")

    def settled(self, operation, payload, plan=None, step=None):
        argv = ["execute", operation, "--input", json.dumps(payload), "--id", payload["request_id"]]
        if plan:
            argv += ["--plan", plan, "--step", step]
        record = self.cli(*argv)
        for _ in range(40):
            if record["status"] in ("completed", "failed", "cancelled"):
                break
            self.rpc("anvil_mine", ["0x2"], "Local fork settlement advancement; no public CLI block-mining operation")
            record = self.cli("status", record["id"])
        assert record["status"] == "completed", record
        duplicate = self.cli(*argv)
        assert duplicate["id"] == record["id"] and duplicate["submission"] == record["submission"]
        return record

    def swap(self, dex, token_in, token_out, amount, fee=100):
        quote = self.read(f"{dex}.quote", {**BASE, "token_in": token_in, "token_out": token_out, "amount": amount, "protocol": "v3", "fee_tier": fee})["data"]
        assert quote["price_impact_bps"] <= 100
        request = {**BASE, "request_id": str(uuid.uuid4()), "token_in": token_in, "token_out": token_out,
                   "amount": amount, "protocol": "v3", "fee_tier": fee, "recipient": self.owner,
                   "deadline": int(time.time()) + 120, "max_fee_eth": "0.01", "slippage_bps": 50,
                   "max_price_impact_bps": 100,
                   "min_output": decimal((int(quote["output"]) * 9950 + 9999) // 10000, quote["token_out"]["decimals"])}
        bounds = {"max_input": amount, "max_fee_eth": "0.01", "max_slippage_bps": 100,
                  "max_price_impact_bps": 100, "allowed_fee_tiers": [fee], "max_deadline_seconds": 600,
                  "minimum_rate": {"output_atoms": str(int(quote["output"]) * 98), "input_atoms": str(int(quote["input"]) * 100)}}
        return request, bounds, quote

    def start(self):
        # Only the archive anchor is read publicly; no private account/RPC data.
        head = FORK.rpc(FORK.URLS[CHAIN], "eth_getBlockByNumber", ["latest", False])
        anchor = FORK.rpc(FORK.URLS[CHAIN], "eth_getBlockByNumber", [hex(int(head["number"], 16) - 128), False])
        self.save("archive-anchor", {"rpc": FORK.URLS[CHAIN], "block": anchor["number"], "hash": anchor["hash"],
                  "reason": "Public archive state needed to bootstrap an owned fork; GoFin has no fork lifecycle operation"})
        lease = allocate_loopback()
        self.endpoint, port = lease.url, lease.port
        lease.release()
        self.node = start_owned_process([str(Path.home() / ".foundry/bin/anvil"), "--host", "127.0.0.1", "--port", str(port), "--chain-id", str(CHAIN), "--fork-url", FORK.URLS[CHAIN], "--fork-block-number", str(int(anchor["number"], 16)), "--timestamp", str(int(time.time())), "--base-fee", "100000000", "--gas-price", "10000000", "--disable-min-priority-fee", "--no-cors", *(["--block-time", "1"] if self.yolo else [])], log_path=self.directory / "anvil.log", marker_path=self.directory / "anvil.owner.json", env=self.env)
        for _ in range(80):
            try:
                self.identity = FORK.rpc(self.endpoint, "anvil_metadata", [])
                break
            except (OSError, RuntimeError):
                time.sleep(.25)
        assert self.identity and self.identity["chainId"] == CHAIN
        assert self.identity["forkedNetwork"]["forkBlockHash"] == anchor["hash"]
        keyfile = self.directory / "PUBLIC-SYNTHETIC-NEVER-FUND.env"
        keyfile.write_text(f"GOFIN_DEV_EVM_PRIVATE_KEY={KEY}\n")
        keyfile.chmod(0o600)
        self.make("dev-env-import", DEV_CREDENTIALS_FILE=str(keyfile), DEV_FLAGS="--json")
        keyfile.unlink()
        worktree_key = hashlib.sha256(str(SOURCE).encode()).hexdigest()[:20]
        config_path = Path(self.env["GOFIN_DEV_STATE_HOME"]) / worktree_key / self.name / "config.json"
        self.config_path = config_path
        config = json.loads(config_path.read_text())
        assert config["environment_id"] == hashlib.sha256(f"{worktree_key}:{self.name}".encode()).hexdigest()
        self.owner = config["evm_addresses"][0]
        config["rpc_urls"] = {str(CHAIN): [self.endpoint]}
        config["reviewed_chain_ids"] = [CHAIN]
        if self.yolo:
            config["yolo_fixture"] = {"enabled": True}
            config["policy"] = {"max_movement_usd_micros": "20000000",
                                "max_run_allocation_usd_micros": "200000000",
                                "preview_ttl_seconds": 90}
        config_path.write_text(json.dumps(config, indent=2) + "\n")
        config_path.chmod(0o600)
        self.rpc("anvil_setBalance", [self.owner, hex(3 * 10**16)], "Synthetic 0.03 ETH local gas/bootstrap funding; GoFin does not mint balances")
        self.started = True
        self.make("dev-env-start", DEV_MODE="live", DEV_FLAGS="--json")
        self.cli("schema", "plans.authorize")
        self.cli("schema", "pancakeswap.swap")
        self.cli("schema", "uniswap.swap")
        if self.return_dex != "uniswap":
            self.cli("schema", f"{self.return_dex}.swap")
        if not self.yolo:
            # Seed >2 USDC through GoFin before the financial baseline, not impersonation.
            seed, _, _ = self.swap("uniswap", "native", TOKEN, "0.002")
            self.settled("uniswap.swap", seed)

    def run_yolo_single(self):
        """Same native-input route as the live test, with only synthetic fork funds."""
        assert self.yolo
        assert self.cli("yolo", "status")["enabled"] is True
        before = self.read("wallet.balances", {**BASE, "tokens": [TOKEN]})
        request, _, quote = self.swap("uniswap", "native", TOKEN, "0.002")
        # Finite fee headroom includes the fork's L1 posting fee estimate;
        # principal plus cap remains below the USD 20 movement ceiling.
        request["max_fee_eth"] = "0.001"
        preview = self.cli("preview", "uniswap.swap", "--input", json.dumps(request))
        assert any(message.startswith("Yolo mode is active") for message in preview["warnings"])
        record = self.settled("uniswap.swap", request)
        assert record["authorization_source"] == "yolo", record
        audit = record["approval"]
        assert audit["source"] == "yolo" and audit["yolo_provenance"]["revision"] > 0
        assert audit["yolo_provenance"]["generation"] and audit["yolo_provenance"]["checked_at"]
        effect = conversion(record)
        assert effect["spent"]["atoms"] == "2000000000000000"
        assert int(effect["received"]["atoms"]) >= int(quote["output"]) * 9950 // 10000
        after = self.read("wallet.balances", {**BASE, "tokens": [TOKEN]})
        def units(balances, reference):
            return int(next(row["amount"]["atoms"] for row in balances["data"]
                            if row["amount"]["asset"]["reference"] == reference))
        costs = sum(int(item["amount"]["atoms"]) for item in record["effects"]["effects"]
                    if item["kind"] == "cost" and item["amount"]["asset"]["reference"] == "native")
        assert units(after, "native") == units(before, "native") - 2 * 10**15 - costs
        assert units(after, TOKEN) == units(before, TOKEN) + int(effect["received"]["atoms"])
        assert self.submissions() == 1
        self.make("dev-env-stop", DEV_FLAGS="--json")
        self.started = False
        config = json.loads(self.config_path.read_text())
        config["yolo_fixture"] = {"enabled": False}
        self.config_path.write_text(json.dumps(config, indent=2) + "\n")
        self.config_path.chmod(0o600)
        self.started = True
        self.make("dev-env-start", DEV_MODE="live", DEV_FLAGS="--json")
        assert self.cli("yolo", "status")["enabled"] is False
        recovered = self.cli("status", record["id"])
        assert recovered["submission"] == record["submission"]
        assert recovered["effects"] == record["effects"]
        rejected = {**request, "request_id": str(uuid.uuid4()), "deadline": int(time.time()) + 120}
        denied = self.cli("execute", "uniswap.swap", "--input", json.dumps(rejected),
                          "--id", rejected["request_id"])
        # A recorded execution failure is a structured operation result, not
        # a transport/argument error; assert its terminal financial outcome.
        assert denied["status"] == "failed" and denied["submission"] is None
        assert denied["error"]["code"] == "AUTHORIZATION_UNAVAILABLE"
        assert self.submissions() == 1
        self.save("yolo-single-result", {"result": "passed", "before": before, "after": after,
                  "operation": record, "disabled_recovery": recovered["id"],
                  "native_cost_atoms": str(costs), "submissions": 1,
                  "authority": "Yolo with ordinary development approval unavailable",
                  "protection": "environment-local fixture; installed sudo isolation not tested",
                  "production_writes": 0})

    def run(self, principal_only):
        baseline = self.snapshot()
        assert int(baseline["token"]["data"]["eip2612"]["nonce"]) == 0
        assert all(int(value["data"]["units"]) == 0 for value in baseline["allowances"].values())
        resource = baseline["balances"]["resource"]
        native = {"network": f"ethereum:{CHAIN}", "reference": "native", "decimals": 18}
        usdc = {"network": f"ethereum:{CHAIN}", "reference": TOKEN, "decimals": 6}
        forward, forward_bounds, fq = self.swap("pancakeswap", TOKEN, "native", "2")
        max_proceeds = (int(fq["output"]) * 101 + 99) // 100
        reverse, reverse_bounds, _ = self.swap(self.return_dex, "native", TOKEN, decimal(max_proceeds, 18), self.return_fee)
        dependency = {"producing_step_id": "sell", "resource": resource, "asset": native}
        if principal_only:
            dependency["funding_scope"] = "principal_only"
        steps = [{"step_id": "sell", "operation": "pancakeswap.swap", "input": forward, "permitted_variations": forward_bounds, "completion": {"operation_succeeded": True}, "funding_dependencies": []},
                 {"step_id": "return", "operation": f"{self.return_dex}.swap", "input": reverse, "permitted_variations": reverse_bounds, "completion": {"operation_succeeded": True}, "funding_dependencies": [dependency]}]
        plan = {"version": 1, "name": "Owned fork full ETH proceeds with baseline gas", "steps": steps,
                "aggregate_bounds": {"maximum_debits": [{"resource": resource, "amount": {"asset": usdc, "atoms": "2000000"}}, {"resource": resource, "amount": {"asset": native, "atoms": str(max_proceeds)}}],
                    "maximum_costs": [{"location": {"kind": "cash", "resource": resource}, "amount": {"asset": native, "atoms": str(2 * FEE_ATOMS)}, "treatment": "additional_debit"}]}}
        review = self.cli("plans", "preview", "--input", json.dumps(plan), "--valid-for", "10m")
        grant = self.cli("plans", "authorize", "--input", json.dumps(plan), "--id", str(uuid.uuid4()), "--review-hash", review["review_hash"], "--valid-for", "10m")
        plan_id = grant["plan_id"]
        before = self.submissions()
        self.cli("execute", f"{self.return_dex}.swap", "--input", json.dumps(reverse), "--id", str(uuid.uuid4()), "--plan", plan_id, "--step", "return", "--dry-run", error=True)
        invalid = {**forward, "amount": "2.1"}
        self.cli("execute", "pancakeswap.swap", "--input", json.dumps(invalid), "--id", str(uuid.uuid4()), "--plan", plan_id, "--step", "sell", "--dry-run", error=True)
        assert self.submissions() == before
        sold = self.settled("pancakeswap.swap", forward, plan_id, "sell")
        proceeds = conversion(sold)["received"]
        assert proceeds["asset"] == native and conversion(sold)["spent"]["atoms"] == "2000000"
        # Reopen the existing journal BETWEEN steps; no fixture reset or new grant.
        status_before_restart = self.cli("plans", "status", plan_id)
        submissions_before_restart = self.submissions()
        self.restart()
        recovered = self.cli("status", sold["id"])
        assert recovered["status"] == "completed"
        assert recovered["submission"] == sold["submission"] and recovered["effects"] == sold["effects"]
        recovered_plan = self.cli("plans", "status", plan_id)
        assert recovered_plan["revision"]["revision_id"] == status_before_restart["revision"]["revision_id"]
        assert recovered_plan["usage"]["verified_proceeds"] == status_before_restart["usage"]["verified_proceeds"]
        assert recovered_plan["next_eligible_step_id"] == "return"
        assert self.submissions() == submissions_before_restart
        self.save("between-steps-recovery", {"forward_operation": sold["id"],
                  "plan": plan_id, "verified_proceeds": recovered_plan["usage"]["verified_proceeds"],
                  "new_submissions": 0, "new_authorizations": 0})
        # Refresh the concrete return; its amount is EXACTLY receipt-proven ETH.
        reverse_template_id = reverse["request_id"]
        reverse, _, _ = self.swap(self.return_dex, "native", TOKEN, decimal(proceeds["atoms"], 18), self.return_fee)
        # Input identity is part of the approved template. Outer --id identifies
        # each concrete attempt independently; refreshing the quote cannot alter it.
        reverse["request_id"] = reverse_template_id
        before = self.submissions()
        if not principal_only:
            self.cli("execute", f"{self.return_dex}.swap", "--input", json.dumps(reverse), "--id", reverse["request_id"], "--plan", plan_id, "--step", "return", error="PLAN_FUNDING_NOT_SETTLED")
            assert self.submissions() == before
            self.cli("plans", "cancel", plan_id)
            self.save("result", {"expected_default_dependency_block": True, "return_submissions": 0,
                      "restoration": "Not established for this deliberate blocked case; owned synthetic fork disposed", "production_writes": 0})
            return
        # Deliberately remove ONLY baseline gas in the OWNED local fixture.
        # This is an admin state mutation, never a financial restoration claim.
        current_balance = self.read("wallet.balances", {**BASE, "tokens": [TOKEN]})
        captured_native = int(next(row["amount"]["atoms"] for row in current_balance["data"]
                                   if row["amount"]["asset"] == native))
        assert captured_native > int(proceeds["atoms"])
        self.rpc("anvil_setBalance", [self.owner, hex(int(proceeds["atoms"]))],
                 "Local negative-test fixture: preserve ALL producer principal, remove baseline gas; GoFin cannot mutate balances")
        try:
            rejection = self.cli("execute", f"{self.return_dex}.swap", "--input", json.dumps(reverse),
                                 "--id", str(uuid.uuid4()), "--plan", plan_id, "--step", "return",
                                 "--dry-run", error=True)
            assert self.submissions() == before
            self.save("insufficient-baseline-gas", {"error_code": rejection["error"]["code"],
                      "native_fixture_atoms": proceeds["atoms"], "all_principal_preserved": True,
                      "new_submissions": 0, "fixture_mutation": "Local admin test only; not a financial write or restoration"})
        finally:
            self.rpc("anvil_setBalance", [self.owner, hex(captured_native)],
                     "Undo ONLY local negative-test fixture, restoring captured native amount; preserve journal and permit nonce")
        restored_balance = self.read("wallet.balances", {**BASE, "tokens": [TOKEN]})
        assert int(next(row["amount"]["atoms"] for row in restored_balance["data"]
                        if row["amount"]["asset"] == native)) == captured_native
        returned = self.settled(f"{self.return_dex}.swap", reverse, plan_id, "return")
        assert conversion(returned)["spent"] == proceeds
        final = self.snapshot()
        assert int(final["token"]["data"]["eip2612"]["nonce"]) == 1
        assert all(int(value["data"]["units"]) == 0 for value in final["allowances"].values())
        def units(snapshot, asset):
            return int(next(row["amount"]["atoms"] for row in snapshot["balances"]["data"] if row["amount"]["asset"] == asset))
        costs = sum(int(effect["amount"]["atoms"]) for record in (sold, returned) for effect in record["effects"]["effects"] if effect["kind"] == "cost")
        assert units(final, native) == units(baseline, native) - costs
        assert units(final, usdc) == units(baseline, usdc) - 2000000 + int(conversion(returned)["received"]["atoms"])
        status = self.cli("plans", "status", plan_id)
        assert all(step["state"] == "completed" for step in status["steps"])
        assert status["next_eligible_step_id"] is None
        assert status["usage"]["reservations"] == []
        native_proceeds = next(item for item in status["usage"]["verified_proceeds"]
                              if item["producing_step_id"] == "sell" and item["asset"] == native)
        assert native_proceeds["available"] == proceeds["atoms"]
        assert native_proceeds["consumed"] == proceeds["atoms"]
        assert all(record["status"] in {"rejected", "expired", "cancelled", "completed", "failed"}
                   for record in self.operations())
        self.restart()
        assert self.submissions() == before + 1
        for original in (sold, returned):
            recovered = self.cli("status", original["id"])
            assert recovered["submission"] == original["submission"] and recovered["effects"] == original["effects"]
        self.cli("plans", "cancel", plan_id)
        self.cli("execute", "pancakeswap.swap", "--input", json.dumps(forward), "--id", str(uuid.uuid4()), "--plan", plan_id, "--step", "sell", error=True)
        assert self.submissions() == before + 1
        self.save("result", {"single_plan": plan_id, "status": status, "baseline": baseline, "final": final,
                  "exact_proceeds_spent": proceeds, "baseline_gas_spent": str(costs), "allowances_restored": True,
                  "permit_nonce_consumed": 1, "between_steps_recovery": True, "insufficient_gas_rejection": rejection["error"]["code"],
                  "fixture_mutation": "Owned local negative-test balance mutation undone exactly; not financial restoration", "no_outstanding_reservations": True, "public_pricing": "normal adapter-internal public CoinGecko reads",
                  "approval": "automatic isolated development; Touch ID not tested", "production_writes": 0})

    def close(self):
        if self.started:
            try:
                self.make("dev-env-stop", DEV_FLAGS="--json")
                self.started = False
            except Exception:
                if self.node:
                    self.node.detach()
                raise RuntimeError("Broker cleanup unresolved; owned node retained for reconciliation")
        if self.node and not self.node.stop():
            self.node.detach()
            raise RuntimeError("Owned node cleanup unresolved; evidence retained")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--return-dex", choices=("uniswap", "sushi"), default="uniswap")
    parser.add_argument("--return-fee", type=int, choices=(100, 500, 3000, 10000), default=100)
    parser.add_argument("--case", choices=("both", "default", "principal"), default="both")
    parser.add_argument("--yolo-single-swap", action="store_true",
                        help="Rehearse one native ETH to USDC swap using the explicit Yolo fixture, then disable and verify recovery")
    args = parser.parse_args()
    (SOURCE / "work").mkdir(exist_ok=True)
    evidence = Path(tempfile.mkdtemp(prefix="amm-cli-plan-fork.", dir=SOURCE / "work"))
    print(json.dumps({"evidence": str(evidence), "production_writes": 0}), flush=True)
    for kind in (("yolo",) if args.yolo_single_swap else
                 (("default", "principal") if args.case == "both" else (args.case,))):
        rehearsal = Rehearsal(evidence, kind, args.return_dex, args.return_fee, yolo=args.yolo_single_swap)
        try:
            rehearsal.start()
            if args.yolo_single_swap:
                rehearsal.run_yolo_single()
            else:
                rehearsal.run(kind == "principal")
            print(json.dumps({"case": kind, "result": "passed", "evidence": str(rehearsal.directory)}), flush=True)
        finally:
            rehearsal.close()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
