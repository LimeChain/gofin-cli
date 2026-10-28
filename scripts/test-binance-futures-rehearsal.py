#!/usr/bin/env python3
"""Public CLI -> broker -> engine -> real Binance adapter -> isolated venue.

Synthetic credentials/prices only; no HTTP, imported accounts or live funds.
The fixture independently tracks native balances and signed position quantities.
"""
from __future__ import annotations

import json
import os
from pathlib import Path
import shutil
import sqlite3
import subprocess
import sys
import tempfile

REPO = Path(__file__).resolve().parent.parent
DEV = REPO / "scripts/gofin-dev.py"
NAME = "binance-futures-rehearsal"
ACCOUNT = "binance-rehearsal"


def run(args: list[str], env: dict[str, str], timeout: int = 240) -> subprocess.CompletedProcess:
    return subprocess.run([sys.executable, str(DEV), *args], cwd=REPO, env=env,
                          capture_output=True, text=True, timeout=timeout)


def lifecycle(action: str, env: dict[str, str], fixture: str = "binance-futures") -> dict:
    args = ["--json", action, NAME]
    if action == "start":
        args += ["--mode", "synthetic", "--fixture", fixture]
    result = run(args, env)
    if result.returncode:
        raise AssertionError(f"{action} failed: {result.stderr or result.stdout}")
    return json.loads(result.stdout)


def cli(args: list[str], env: dict[str, str], unresolved: bool = False) -> dict:
    result = run(["cli", NAME, "--", *args, "--json"], env, timeout=60)
    assert "\x1b" not in result.stdout, "Machine output contains terminal decoration"
    try:
        value = json.loads(result.stdout)
    except ValueError as error:
        raise AssertionError(f"CLI returned invalid JSON for {args[:2]}: {result.stdout} {result.stderr}") from error
    if result.returncode and not unresolved:
        raise AssertionError(f"CLI failed for {args[:2]}: {json.dumps(value)} {result.stderr}")
    return value


def rehearse(fixture: str, *, positive_only: bool = False) -> dict:
    standalone_subaccount = fixture == "binance-futures-subaccount"
    account = "binance-subaccount-rehearsal" if standalone_subaccount else ACCOUNT
    with tempfile.TemporaryDirectory(prefix="gofin-binance-rehearsal-") as temp:
        state_home = Path(temp) / "state"
        env = dict(os.environ, GOFIN_DEV_STATE_HOME=str(state_home))
        records: dict[str, dict] = {}
        started = False
        try:
            startup = lifecycle("start", env, fixture)
            started = True
            fixture_path, = state_home.rglob("binance-fixture.json")
            metadata_path, = state_home.rglob("environment.json")
            metadata = json.loads(metadata_path.read_text())
            if positive_only:
                # Disable the fixture's deliberate first-order timeout before any
                # request, so this scenario tests the uninterrupted happy path.
                fixture_state = json.loads(fixture_path.read_text())
                fixture_state["lost_response_injected"] = True
                fixture_state["suppress_faults"] = True
                lifecycle("stop", env)
                fixture_path.write_text(json.dumps(fixture_state))
                lifecycle("start", env, fixture)

            def state() -> dict:
                return json.loads(fixture_path.read_text())

            def reservations() -> int:
                with sqlite3.connect(Path(metadata["database"]).as_uri() + "?mode=ro", uri=True) as db:
                    return db.execute("SELECT COUNT(*) FROM reservations").fetchone()[0]

            def execute(label: str, operation: str, data: dict, lost: bool = False) -> dict:
                data = {"request_id": label, **data}
                serialized = json.dumps(data)
                before = state()["posts"]
                preview = cli(["preview", operation, "--input", serialized], env)
                assert preview.get("operation") == operation, preview
                assert state()["posts"] == before, "Preview dispatched a financial write"
                args = ["execute", operation, "--input", serialized, "--id", label]
                result = cli(args, env, unresolved=lost)
                records[label] = result
                if lost:
                    assert result.get("status") in {"unknown", "submitted", "pending"}, result
                    identity = result["submission"]["reference"]
                    assert reservations() > 0, "Uncertain order lost its reservations"
                    posts = state()["posts"]
                    duplicate = cli(args, env, unresolved=True)
                    assert duplicate["id"] == result["id"], "Duplicate changed operation identity"
                    assert duplicate["submission"]["reference"] == identity
                    assert duplicate["status"] in {"unknown", "submitted", "pending"}
                    assert state()["posts"] == posts, "Duplicate replayed the uncertain order"
                    lifecycle("stop", env)
                    lifecycle("start", env, fixture)
                    result = cli(["status", result["id"]], env)
                    assert result["submission"]["reference"] == identity
                    assert state()["posts"] == posts, "Recovery dispatched another order"
                    records[label + "-recovered"] = result
                assert result.get("status") == "completed", result
                assert result.get("run_id") is None, result
                assert result.get("effects", {}).get("proof_digest"), "Completion omitted durable proof"
                assert result.get("approval", {}).get("mechanism", "").startswith("development"), result
                assert reservations() == 0, "Settled operation left reservations"
                posts = state()["posts"]
                duplicate = cli(args, env)
                assert duplicate["id"] == result["id"]
                assert duplicate["submission"]["reference"] == result["submission"]["reference"]
                assert state()["posts"] == posts, "Completed duplicate was resubmitted"
                return result

            cli(["capabilities"], env)
            cli(["connections", "list"], env)
            cli(["policy", "show"], env)
            for operation in ["binance.futures.transfer", "binance.futures.leverage-set",
                              "binance.futures.margin-type-set", "binance.futures.order-place"]:
                cli(["schema", operation], env)
                cli(["explain", operation], env)
            balances = cli(["read", "binance.balances", "--input", json.dumps({"account": account})], env)
            assert balances["balances"][0]["free"] == "100"
            assert state()["futures"] == "0" and state()["position"] == "0"

            def transfer(label: str, direction: str, amount: str, delayed_history: bool = False) -> dict:
                result = execute(label, "binance.futures.transfer", {
                    "authority": "self" if standalone_subaccount else account, "account": account,
                    "direction": direction, "amount": amount,
                }, lost=delayed_history)
                if standalone_subaccount:
                    receipt = result.get("dispatch_evidence")
                    assert receipt and receipt["route"] == "self_wallet" and receipt["venueId"], result
                    native = [row for row in state()["transfers"] if str(row["tranId"]) == receipt["venueId"]]
                    assert len(native) == 1 and native[0]["status"] == "CONFIRMED", native
                    assert "clientTranId" not in native[0], "Fixture fabricated unsupported client ID proof"
                return result

            for index, (mode, opening_side, closing_side) in enumerate([
                    ("ISOLATED", "BUY", "SELL"), ("CROSSED", "SELL", "BUY")]):
                prefix = "isolated-long" if index == 0 else "cross-short"
                deposit = transfer(prefix + "-deposit", "SPOT_TO_FUTURES", "10", delayed_history=standalone_subaccount and index == 0 and not positive_only)
                assert state()["futures"] == "10" and state()["position"] == "0"
                assert any(effect["kind"] == "transfer" for effect in deposit["effects"]["effects"])
                if index == 0:
                    execute("set-leverage", "binance.futures.leverage-set", {
                        "account": account, "symbol": "ETHUSDT", "leverage": 3,
                    })
                execute(prefix + "-set-margin", "binance.futures.margin-type-set", {
                    "account": account, "symbol": "ETHUSDT", "margin_type": mode,
                })
                order = {
                    "account": account, "symbol": "ETHUSDT", "quantity": "0.003",
                    "limit_price": "2000", "time_in_force": "FOK", "leverage": 3,
                    "margin_type": mode, "maximum_fees": {"USDT": "0.01"},
                    "maximum_loss": "0.01", "maximum_profit": "0.01",
                    "maximum_funding_loss": "0.01", "maximum_funding_profit": "0.01",
                }
                opened = execute(prefix + "-open", "binance.futures.order-place",
                                 {**order, "side": opening_side, "reduce_only": False}, lost=index == 0 and not positive_only)
                assert state()["position"] == ("0.003" if index == 0 else "-0.003")
                assert state()["futures"] == "9.997", "Opening fee was not applied exactly once"
                expected_fees = [(opened, 3)]
                if index == 0:
                    reduced = execute(prefix + "-reduce", "binance.futures.order-place",
                                      {**order, "side": closing_side, "quantity": "0.001", "reduce_only": True})
                    assert state()["position"] == "0.002", "First reduction must preserve the remaining exposure"
                    assert state()["futures"] == "9.996", "Partial reduction commission was not exact"
                    expected_fees.append((reduced, 1))
                    closing_quantity, closing_fee = "0.002", 2
                else:
                    closing_quantity, closing_fee = "0.003", 3
                closed = execute(prefix + "-close", "binance.futures.order-place",
                                 {**order, "side": closing_side, "quantity": "all" if positive_only else closing_quantity,
                                  **({"maximum_quantity": closing_quantity} if positive_only else {}), "reduce_only": True})
                expected_fees.append((closed, closing_fee))
                assert opened["submission"]["reference"] != closed["submission"]["reference"]
                if positive_only:
                    assert closed["preview"]["binding"]["quantity"] == closing_quantity
                assert state()["position"] == "0" and state()["futures"] == ("9.993" if positive_only else "9.994")
                for record, fee_milliusdt in expected_fees:
                    effects = record["effects"]["effects"]
                    assert any(effect["kind"] == "position_change" for effect in effects), effects
                    fee = [effect for effect in effects if effect["kind"] == "cost" and effect.get("category") == "exchange_commission"]
                    assert len(fee) == 1 and fee[0]["amount"]["atoms"] == str(fee_milliusdt * 10**33), fee
                if positive_only:
                    pnl = [effect for effect in closed["effects"]["effects"]
                           if effect.get("kind") == "cost" and effect.get("category") == "realized_loss"]
                    assert len(pnl) == 1 and pnl[0]["amount"]["atoms"] == str(10**33), pnl
                transfer(prefix + "-return", "FUTURES_TO_SPOT", "9.993" if positive_only else "9.994")
                assert state()["futures"] == "0" and state()["position"] == "0"

            final = state()
            assert final["spot"] == ("99.986" if positive_only else "99.988"), "Final spot must equal initial less five exact commissions"
            assert final["order_posts"] == 5 and final["transfer_posts"] == 4 and final["posts"] == 12
            assert final["leverage"] == 3 and final["margin_type"] == "CROSSED"
            balances = cli(["read", "binance.balances", "--input", json.dumps({"account": account})], env)
            assert balances["balances"][0]["free"] == ("99.986" if positive_only else "99.988")
            lifecycle("stop", env)
            lifecycle("start", env, fixture)
            for label, record in records.items():
                if record["status"] != "completed":
                    continue
                recovered = cli(["status", record["id"]], env)
                assert recovered["status"] == "completed"
                assert recovered["effects"]["proof_digest"] == record["effects"]["proof_digest"]
            assert state()["posts"] == 12 and reservations() == 0
            assert final["wallet_transfer_posts"] == (4 if standalone_subaccount else 0)
            assert final["universal_transfer_posts"] == (0 if standalone_subaccount else 4)
            uncertain_case = None
            if standalone_subaccount and not positive_only:
                # A distinct fault after both restored cycles: the venue executes
                # a fifth transfer but drops its response, so no native tranId
                # reaches the durable journal. Matching amount/time is not proof.
                lost_input = json.dumps({"request_id":"child-transfer-response-lost", "authority":"self", "account":account, "direction":"SPOT_TO_FUTURES", "amount":"1"})
                lost_args = ["execute", "binance.futures.transfer", "--input", lost_input, "--id", "child-transfer-response-lost"]
                unknown = cli(lost_args, env, unresolved=True)
                records["child-transfer-response-lost"] = unknown
                assert unknown["status"] == "unknown", unknown
                assert not unknown.get("dispatch_evidence"), "Lost response fabricated a durable native receipt"
                assert reservations() > 0
                posts = state()["posts"]
                assert posts == 13 and state()["wallet_transfer_posts"] == 5
                assert state()["futures"] == "1" and state()["spot"] == "98.988"
                identity = unknown["submission"]["reference"]
                lifecycle("stop", env)
                lifecycle("start", env, fixture)
                for args in [["status", unknown["id"]], lost_args]:
                    still_unknown = cli(args, env, unresolved=True)
                    assert still_unknown["status"] == "unknown", still_unknown
                    assert still_unknown["submission"]["reference"] == identity
                    assert still_unknown["approval"] == unknown["approval"]
                    assert not still_unknown.get("dispatch_evidence")
                    assert not still_unknown.get("effects")
                    assert state()["posts"] == posts and reservations() > 0
                uncertain_case = {"status":"unknown", "reason":"native transfer response lost; no durable tranId", "native_transfer_rows":5, "new_posts_after_restart":0, "reservation_retained":True, "synthetic_futures_usdt":"1"}
            return {
                "evidence_class": "public_cli_synthetic", "live_network": False,
                "authorization": "development automatic approval", "build_id": startup.get("buildId"),
                "fixture": fixture, "account_role": "standalone subaccount" if standalone_subaccount else "main",
                "transfer_route": "native same-account wallet transfer with returned tranId" if standalone_subaccount else "main universal transfer with clientTranId",
                "master_credentials_used": False if standalone_subaccount else "synthetic main only",
                "child_transfer_delayed_history_recovered": standalone_subaccount and not positive_only,
                "native_history_whole_second_timestamp": standalone_subaccount,
                "split_close_eth": ["0.001", "0.002"],
                "deliberate_lost_transfer_response_case": uncertain_case,
                "scenarios": ["isolated leveraged long with two reduce-only closes", "cross leveraged short"],
                "sequence": "deposit, leverage/margin, open, reduce-only close, return margin",
                "lost_response_recovered_after_restart": not positive_only, "duplicates_dispatched": 0,
                "restored_round_trips_before_separate_fault": {
                    "spot_usdt": final["spot"], "futures_usdt": final["futures"],
                    "position_eth": final["position"], "paid_commission_usdt": "0.012", "realized_loss_usdt": "0.002" if positive_only else "0",
                    "financial_posts": final["posts"], "remaining_reservations": 0,
                },
                "completed_operations": len([r for r in records.values() if r["status"] == "completed"]),
            }
        except Exception:
            evidence = Path(tempfile.mkdtemp(prefix="gofin-binance-rehearsal-failure-"))
            if started:
                lifecycle("stop", env)
                started = False
            if state_home.exists():
                shutil.copytree(state_home, evidence / "synthetic-state")
            (evidence / "operations.json").write_text(json.dumps(records, indent=2))
            print(f"Synthetic failure evidence: {evidence}", file=sys.stderr)
            raise
        finally:
            if started:
                lifecycle("stop", env)


def main() -> int:
    options = sys.argv[1:]
    if options not in ([], ["--positive-main"], ["--positive-subaccount"]):
        raise SystemExit("usage: test-binance-futures-rehearsal.py [--positive-main|--positive-subaccount]")
    positive_only = bool(options)
    fixtures = (["binance-futures"] if options == ["--positive-main"] else
                ["binance-futures-subaccount"] if options == ["--positive-subaccount"] else
                ["binance-futures", "binance-futures-subaccount"])
    reports = [rehearse(fixture, positive_only=positive_only) for fixture in fixtures]
    print(json.dumps({"evidence_class":"public_cli_synthetic", "live_network":False, "rehearsals":reports}, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
