"""Named-plan rehearsal for a future-funded Perps TP/SL lifecycle.

This helper deliberately talks only to the public synthetic GoFin CLI. It does
not call the fixture transport or financial adapters directly.
"""
from __future__ import annotations

import json
from pathlib import Path
import sqlite3
import subprocess
import sys

REPO = Path(__file__).resolve().parent.parent
DEV = REPO / "scripts/gofin-dev.py"
USDC_MINT = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"


def _call(environment: str, args: list[str], env: dict[str, str]) -> dict:
    result = subprocess.run(
        [sys.executable, str(DEV), "cli", environment, "--", *args],
        cwd=REPO,
        env=env,
        capture_output=True,
        text=True,
        timeout=60,
    )
    try:
        value = json.loads(result.stdout)
    except json.JSONDecodeError as error:
        raise RuntimeError(
            f"CLI did not return JSON for {' '.join(args[:2])}: "
            f"{result.stderr or result.stdout}"
        ) from error
    if not isinstance(value, dict):
        raise RuntimeError(f"CLI returned a non-object for {' '.join(args[:2])}.")
    if result.returncode:
        # Status/reconciliation can return a structured non-success result.
        if args[0] != "status":
            raise RuntimeError(
                f"CLI command failed ({' '.join(args[:2])}): "
                f"{result.stderr or result.stdout}"
            )
    return value


def _find_binding_value(value: object, field: str) -> str | None:
    if isinstance(value, dict):
        binding = value.get("binding")
        if isinstance(binding, dict) and isinstance(binding.get(field), str):
            return binding[field]
        for child in value.values():
            found = _find_binding_value(child, field)
            if found:
                return found
    elif isinstance(value, list):
        for child in value:
            found = _find_binding_value(child, field)
            if found:
                return found
    return None


def _plan(wallet: str, position: str) -> dict:
    resource = f"solana:mainnet-beta:{wallet}"
    native = {"network": "solana:mainnet-beta", "reference": "native", "decimals": 9}
    usdc = {"network": "solana:mainnet-beta", "reference": USDC_MINT, "decimals": 6}
    margin = {"network": "solana:mainnet-beta", "reference": "jupiter:margin-usd", "decimals": 6}
    network = {"network": "solana", "cluster": "mainnet-beta", "wallet": "solana-rehearsal"}

    open_input = {
        **network,
        "market": "SOL",
        "side": "long",
        "funding_token": "USDC",
        "collateral_amount_atoms": "11000000",
        "size_usd_atoms": "12100000",
        "maximum_protocol_fee_atoms": "5000000",
        "maximum_protocol_fee_usd_atoms": "500000",
        "maximum_slippage_bps": 50,
        "maximum_network_fee_lamports": "10000",
        "maximum_account_funding_lamports": "6000000",
    }
    open_scope = {
        "maximum_size_usd_atoms": "12100000",
        "maximum_collateral_atoms": "11000000",
        "maximum_protocol_fee_usd_atoms": "500000",
        "maximum_protocol_fee_atoms": "5000000",
        "maximum_slippage_bps": 50,
        "maximum_network_fee_lamports": 10000,
        "maximum_account_funding_lamports": 6000000,
    }
    create_input = {
        **network,
        # The PDA is derived from wallet/market/side by the adapter. This lets
        # the approved plan be reviewed before the position exists.
        "market": "SOL",
        "side": "long",
        "request_type": "sl",
        "trigger_price_usd_atoms": "90000000",
        "receive_token": "USDC",
        "entire_position": True,
        "maximum_protocol_fee_usd_atoms": "500000",
        "maximum_network_fee_lamports": "15000",
        "maximum_account_funding_lamports": "6000000",
    }
    create_scope = {
        "maximum_size_usd_atoms": "12100000",
        "maximum_collateral_usd_atoms": "11000000",
        "maximum_protocol_fee_usd_atoms": "500000",
        "maximum_network_fee_lamports": 15000,
        "maximum_account_funding_lamports": 6000000,
    }
    update_input = {
        **network,
        "request": None,
        "original_operation_id": None,
        "original_submission": None,
        "trigger_price_usd_atoms": "91000000",
        "maximum_network_fee_lamports": "15000",
    }
    update_scope = {
        "maximum_network_fee_lamports": 15000,
        "minimum_trigger_price_usd_atoms": "91000000",
        "maximum_trigger_price_usd_atoms": "91000000",
        "recovery_parent_step_id": "create",
    }
    cancel_input = {
        **network,
        "request": None,
        "original_operation_id": None,
        "original_submission": None,
        "maximum_network_fee_lamports": "15000",
    }
    cancel_scope = {
        "maximum_network_fee_lamports": 15000,
        "recovery_parent_step_id": "create",
    }
    close_input = {
        **network,
        "position": position,
        "receive_token": "USDC",
        "size_usd_atoms": "12100000",
        "minimum_receive_atoms": "10943010",
        "maximum_protocol_fee_usd_atoms": "500000",
        "maximum_slippage_bps": 50,
        "maximum_network_fee_lamports": "10000",
        "maximum_account_funding_lamports": "6000000",
    }
    close_scope = {
        "maximum_size_usd_atoms": "12100000",
        "maximum_collateral_atoms": "11000000",
        "minimum_receive_per_size_numerator": "9",
        "minimum_receive_per_size_denominator": "10",
        "maximum_protocol_fee_usd_atoms": "500000",
        "maximum_protocol_fee_atoms": "5000000",
        "maximum_slippage_bps": 50,
        "maximum_network_fee_lamports": 10000,
        "maximum_account_funding_lamports": 6000000,
    }

    return {
        "version": 1,
        "name": "Synthetic future-funded Perps protection and recovery",
        "steps": [
            {
                "step_id": "open",
                "operation": "jupiter.perps.increase_market",
                "input": open_input,
                "permitted_variations": open_scope,
                "completion": {"operation_succeeded": True},
                "funding_dependencies": [],
            },
            {
                "step_id": "create",
                "operation": "jupiter.perps.tpsl.create",
                "input": create_input,
                "permitted_variations": create_scope,
                "completion": {"operation_succeeded": True},
                "funding_dependencies": [],
            },
            {
                "step_id": "update",
                "operation": "jupiter.perps.tpsl.update",
                "input": update_input,
                "permitted_variations": update_scope,
                "completion": {"operation_succeeded": True},
                "funding_dependencies": [],
            },
            {
                "step_id": "cancel",
                "operation": "jupiter.perps.tpsl.cancel",
                "input": cancel_input,
                "permitted_variations": cancel_scope,
                "completion": {"operation_succeeded": True},
                "funding_dependencies": [],
            },
            {
                "step_id": "close",
                "operation": "jupiter.perps.decrease_market",
                "input": close_input,
                "permitted_variations": close_scope,
                "completion": {"operation_succeeded": True},
                "funding_dependencies": [],
            },
        ],
        "aggregate_bounds": {
            "maximum_debits": [
                {"resource": resource, "amount": {"asset": usdc, "atoms": "11000000"}},
                {"resource": resource, "amount": {"asset": native, "atoms": "18000000"}},
            ],
            "maximum_costs": [
                {
                    "location": {"kind": "cash", "resource": resource},
                    "amount": {"asset": native, "atoms": "65000"},
                    "treatment": "additional_debit",
                },
                {
                    "location": {
                        "kind": "position",
                        "resource": resource,
                        "position": f"{position}:collateral",
                        "position_kind": "locked_collateral",
                    },
                    "amount": {"asset": margin, "atoms": "500000"},
                    "treatment": "included_in_debit",
                },
                {
                    "location": {
                        "kind": "position",
                        "resource": resource,
                        "position": f"{position}:collateral",
                        "position_kind": "locked_collateral",
                    },
                    "amount": {"asset": margin, "atoms": "500000"},
                    "treatment": "additional_debit",
                },
            ],
        },
    }


def test_named_plan_shape() -> None:
    plan = _plan("synthetic-wallet", "synthetic-position")
    steps = plan["steps"]
    assert [step["operation"] for step in steps] == [
        "jupiter.perps.increase_market",
        "jupiter.perps.tpsl.create",
        "jupiter.perps.tpsl.update",
        "jupiter.perps.tpsl.cancel",
        "jupiter.perps.decrease_market",
    ]
    assert "position" not in steps[1]["input"]
    assert steps[1]["input"]["market"] == "SOL" and steps[1]["input"]["side"] == "long"
    assert steps[2]["input"]["request"] is None
    assert steps[2]["permitted_variations"]["recovery_parent_step_id"] == "create"
    assert steps[3]["permitted_variations"]["recovery_parent_step_id"] == "create"
    assert steps[4]["input"]["position"] == "synthetic-position"
    assert plan["aggregate_bounds"]["maximum_debits"][0]["amount"]["atoms"] == "11000000"
    margin_costs = [
        row for row in plan["aggregate_bounds"]["maximum_costs"]
        if row["amount"]["asset"]["reference"] == "jupiter:margin-usd"
    ]
    assert {row["treatment"] for row in margin_costs} == {
        "included_in_debit", "additional_debit"
    }
    assert all(row["amount"]["atoms"] == "500000" for row in margin_costs)


def _network_fee(record: dict) -> int:
    effects = (record.get("effects") or {}).get("effects") or []
    return sum(
        int((row.get("amount") or {}).get("atoms", "0"))
        for row in effects
        if row.get("kind") == "cost" and row.get("category") == "solana_network_fee"
    )


def _token_amount(snapshot: dict, mint: str) -> int:
    return sum(
        int(row.get("amount_atoms", "0"))
        for row in snapshot.get("token_accounts", [])
        if (row.get("asset") or {}).get("reference") == mint
    )


def _summary(record: dict) -> dict:
    return {
        "id": record.get("id"),
        "status": record.get("status"),
        "error_code": (record.get("error") or {}).get("code"),
        "proof_digest": (record.get("effects") or {}).get("proof_digest"),
        "network_fee_lamports": str(_network_fee(record)),
    }


def _restart(environment: str, env: dict[str, str]) -> None:
    stop = subprocess.run(
        [sys.executable, str(DEV), "stop", environment],
        cwd=REPO, env=env, capture_output=True, text=True, timeout=60,
    )
    if stop.returncode:
        raise RuntimeError(stop.stderr or stop.stdout or "Synthetic stop failed.")
    start = subprocess.run(
        [sys.executable, str(DEV), "start", environment, "--mode", "synthetic", "--fixture", "solana"],
        cwd=REPO, env=env, capture_output=True, text=True, timeout=180,
    )
    if start.returncode:
        raise RuntimeError(start.stderr or start.stdout or "Synthetic restart failed.")


def _reservation_count(env: dict[str, str], environment: str) -> int:
    state_home = Path(env["GOFIN_DEV_STATE_HOME"])
    for metadata_path in state_home.rglob("environment.json"):
        try:
            metadata = json.loads(metadata_path.read_text(encoding="utf-8"))
            if metadata.get("name") != environment:
                continue
            database = Path(metadata["database"]).resolve(strict=True)
            with sqlite3.connect(f"{database.as_uri()}?mode=ro", uri=True, timeout=10) as db:
                return int(db.execute("SELECT COUNT(*) FROM reservations").fetchone()[0])
        except (OSError, KeyError, ValueError, TypeError, sqlite3.Error):
            continue
    raise AssertionError("Could not inspect the synthetic plan journal reservations.")


def run_named_plan_tpsl(
    environment_name: str,
    env: dict[str, str],
    initial_usdc_atoms: int,
    initial_lamports: int,
) -> dict[str, object]:
    """Authorize and execute a future-funded open/create/update/cancel/close plan.

    The original TP/SL create remains Pending until the linked cancel child is
    finalized. The function reconciles that parent, then executes the plan's
    final exactly bounded close step to restore the created position and funds.
    """
    scope_input = {
        "network": "solana", "cluster": "mainnet-beta", "wallet": "solana-rehearsal",
    }
    open_input = {
        **scope_input, "market": "SOL", "side": "long", "funding_token": "USDC",
        "collateral_amount_atoms": "11000000", "size_usd_atoms": "12100000",
        "maximum_protocol_fee_atoms": "5000000", "maximum_protocol_fee_usd_atoms": "500000",
        "maximum_slippage_bps": 50, "maximum_network_fee_lamports": "10000",
        "maximum_account_funding_lamports": "6000000",
    }
    candidate = _call(environment_name, [
        "preview", "jupiter.perps.increase_market", "--input", json.dumps(open_input), "--json",
    ], env)
    position = _find_binding_value(candidate, "position")
    if not position:
        raise AssertionError("Read-only market preview omitted the deterministic future Position PDA.")

    # The public wallet snapshot supplies the account identity used in plan
    # resource bounds; no local configuration or signing material is read.
    balances = _call(environment_name, ["read", "--input", json.dumps(scope_input), "wallet.balances", "--json"], env)
    wallet = balances.get("wallet") or _find_binding_value(candidate, "wallet_address")
    if not isinstance(wallet, str):
        raise AssertionError("Public preview/read did not expose the selected wallet address.")
    plan_value = _plan(wallet, position)
    plan_input = json.dumps(plan_value, separators=(",", ":"))
    preview = _call(environment_name, ["plans", "preview", "--input", plan_input, "--valid-for", "10m", "--json"], env)
    review_hash = preview.get("review_hash")
    if not isinstance(review_hash, str) or not review_hash:
        raise AssertionError(f"Named TPSL plan preview omitted its review hash: {json.dumps(preview, sort_keys=True)}")
    approved = _call(environment_name, [
        "plans", "authorize", "--input", plan_input, "--id", "solana-tpsl-future-plan",
        "--valid-for", "10m", "--review-hash", review_hash, "--json",
    ], env)
    plan_id = approved.get("plan_id") or approved.get("id")
    if not isinstance(plan_id, str) or not plan_id:
        raise AssertionError(f"Plan authorization omitted its durable ID: {json.dumps(approved, sort_keys=True)}")

    records: dict[str, dict] = {}
    opened = _call(environment_name, [
        "execute", "jupiter.perps.increase_market", "--input", json.dumps(open_input),
        "--id", "solana-tpsl-plan-open", "--plan", plan_id, "--step", "open", "--json",
    ], env)
    records["open"] = opened
    if opened.get("status") != "completed" or _network_fee(opened) != 5_000:
        raise AssertionError(f"Plan-funded position did not open with exact settlement: {json.dumps(opened, sort_keys=True)}")

    create_payload = plan_value["steps"][1]["input"]
    created = _call(environment_name, [
        "execute", "jupiter.perps.tpsl.create", "--input", json.dumps(create_payload),
        "--id", "solana-tpsl-plan-create", "--plan", plan_id, "--step", "create", "--json",
    ], env)
    records["create"] = created
    if created.get("status") != "pending" or not (created.get("effects") or {}).get("proof_digest"):
        raise AssertionError(f"Future-funded TP/SL create did not prove an open request: {json.dumps(created, sort_keys=True)}")
    if _network_fee(created) != 15_000:
        raise AssertionError("Named-plan TP/SL placement did not retain its exact wallet-paid fee.")
    create_id = created.get("id")
    binding = (created.get("preview") or {}).get("binding") or {}
    request = binding.get("request")
    if not isinstance(create_id, str) or not isinstance(request, str):
        raise AssertionError("Named-plan TP/SL create omitted its durable operation/request identity.")

    updated = _call(environment_name, [
        "execute", "jupiter.perps.tpsl.update", "--input", json.dumps(plan_value["steps"][2]["input"]),
        "--id", "solana-tpsl-plan-update", "--plan", plan_id, "--step", "update", "--json",
    ], env)
    records["update"] = updated
    if updated.get("status") != "completed" or _network_fee(updated) != 15_000:
        raise AssertionError(f"The linked plan update did not settle: {json.dumps(updated, sort_keys=True)}")

    transactions_before = len(json.loads(Path(env["GOFIN_DEV_SOLANA_FIXTURE_STATE"]).read_text()).get("transactions", []))
    _restart(environment_name, env)
    after_restart = _call(environment_name, ["status", create_id, "--json"], env)
    if after_restart.get("status") != "pending" or after_restart.get("error") is not None:
        raise AssertionError(f"The original TP/SL must remain verified/open after restart: {json.dumps(after_restart, sort_keys=True)}")
    transactions_after = len(json.loads(Path(env["GOFIN_DEV_SOLANA_FIXTURE_STATE"]).read_text()).get("transactions", []))
    if transactions_after != transactions_before:
        raise AssertionError("Reconciliation after restart broadcast or duplicated a transaction.")

    cancelled = _call(environment_name, [
        "execute", "jupiter.perps.tpsl.cancel", "--input", json.dumps(plan_value["steps"][3]["input"]),
        "--id", "solana-tpsl-plan-cancel", "--plan", plan_id, "--step", "cancel", "--json",
    ], env)
    records["cancel"] = cancelled
    if cancelled.get("status") != "completed" or _network_fee(cancelled) != 5_000:
        raise AssertionError(f"The linked TP/SL recovery cancel did not settle: {json.dumps(cancelled, sort_keys=True)}")
    parent = _call(environment_name, ["status", create_id, "--json"], env)
    records["cancelled_parent"] = parent
    if parent.get("status") != "completed" or parent.get("error") is not None or not (parent.get("effects") or {}).get("proof_digest"):
        raise AssertionError(f"The create parent did not reconcile the exact cancel/refund: {json.dumps(parent, sort_keys=True)}")

    close_input = plan_value["steps"][4]["input"]
    closed = _call(environment_name, [
        "execute", "jupiter.perps.decrease_market", "--input", json.dumps(close_input),
        "--id", "solana-tpsl-plan-close", "--plan", plan_id, "--step", "close", "--json",
    ], env)
    records["close"] = closed
    if closed.get("status") != "completed" or _network_fee(closed) != 5_000:
        raise AssertionError(f"The final bounded position close did not settle: {json.dumps(closed, sort_keys=True)}")

    final_balances = _call(environment_name, ["read", "--input", json.dumps(scope_input), "wallet.balances", "--json"], env)
    final_positions = _call(environment_name, ["read", "--input", json.dumps(scope_input), "jupiter.perps.positions", "--json"], env)
    expected_usdc = initial_usdc_atoms - 2_000
    expected_lamports = initial_lamports - 45_000
    observed_usdc = _token_amount(final_balances, USDC_MINT)
    observed_lamports = int((final_balances.get("native") or {}).get("amount_atoms", -1))
    if observed_usdc != expected_usdc:
        raise AssertionError(f"Named-plan TPSL restoration USDC expected {expected_usdc}, observed {observed_usdc}.")
    if observed_lamports != expected_lamports:
        raise AssertionError(f"Named-plan TPSL restoration lamports expected {expected_lamports}, observed {observed_lamports}.")
    if any(row.get("position") == position for row in final_positions.get("positions", [])):
        raise AssertionError("The named-plan TPSL rehearsal left its exact position open.")

    plan_status = _call(environment_name, ["plans", "status", plan_id, "--json"], env)
    if plan_status.get("next_eligible_step_id") is not None:
        raise AssertionError(f"Named plan did not reach a terminal successful state: {json.dumps(plan_status, sort_keys=True)}")
    reservations = _reservation_count(env, environment_name)
    if reservations != 0:
        raise AssertionError("The named-plan TPSL terminal path left plan reservations behind.")
    return {
        "plan_id": plan_id,
        "position": position,
        "request": request,
        "steps": {name: _summary(record) for name, record in records.items()},
        "plan_status": plan_status,
        "reservation_count": reservations,
        "restoration": {
            "expected_usdc_atoms": str(expected_usdc),
            "observed_usdc_atoms": str(observed_usdc),
            "expected_lamports": str(expected_lamports),
            "observed_lamports": str(observed_lamports),
            "open_positions": len(final_positions.get("positions", [])),
        },
    }
