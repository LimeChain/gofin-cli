"""Public-CLI Trigger partial/full fill lifecycle for the synthetic Solana fixture."""

from __future__ import annotations

import json
from pathlib import Path
import sqlite3
import time
from typing import Callable


def run_partial_then_full_fill(
    environment_name: str,
    env: dict[str, str],
    wallet_address: str,
    starting_balances: dict,
    input_mint: str,
    output_mint: str,
    call: Callable,
    call_pending: Callable,
    summary: Callable,
    wallet_token_amount: Callable,
) -> dict:
    """Exercise create → partial fill → full fill → durable parent settlement."""
    fill_control = Path(env["GOFIN_DEV_SOLANA_TRIGGER_FILL_FILE"])
    order_input = {
        "wallet": "solana-rehearsal",
        "network": "solana",
        "cluster": "mainnet-beta",
        "input_mint": input_mint,
        "output_mint": output_mint,
        "making_amount_atoms": "100000",
        "taking_amount_atoms": "99000",
        "maximum_network_fee_lamports": "10000",
        "maximum_account_funding_lamports": "5000000",
    }
    created = call(
        environment_name,
        [
            "execute",
            "jupiter.limit.create",
            "--input",
            json.dumps(order_input),
            "--id",
            "solana-trigger-partial-full-create",
            "--json",
        ],
        env,
    )
    if created.get("status") != "pending" or not (created.get("effects") or {}).get("proof_digest"):
        raise AssertionError(
            "Trigger placement must persist its exact escrow proof and remain pending until fill: "
            + json.dumps(created, sort_keys=True)
        )
    operation_id = created.get("id")
    order_key = created.get("order_key") or (created.get("preview") or {}).get("binding", {}).get("order_key")
    if not isinstance(operation_id, str) or not isinstance(order_key, str):
        raise AssertionError("Trigger placement omitted its durable operation or exact order identity.")

    status_input = {
        "wallet": "solana-rehearsal",
        "network": "solana",
        "cluster": "mainnet-beta",
        "order_key": order_key,
    }

    def read_order() -> dict:
        return call_pending(
            environment_name,
            ["read", "--input", json.dumps(status_input), "jupiter.limit.status", "--json"],
            env,
        )

    opened = read_order()
    if opened.get("state") != "open_escrowed" or opened.get("remaining_making_atoms") != "100000":
        raise AssertionError("Trigger did not expose its exact initial escrow remainder: " + json.dumps(opened, sort_keys=True))

    def fill(making: int, taking: int, close_rent_accounts: bool = False) -> None:
        fill_control.write_text(
            json.dumps(
                {
                    "order_key": order_key,
                    "making_atoms": str(making),
                    "taking_atoms": str(taking),
                    "close_rent_accounts": close_rent_accounts,
                },
                separators=(",", ":"),
            ),
            encoding="utf-8",
        )

    fill(40_000, 39_600)
    partial = read_order()
    if (
        partial.get("state") != "open_escrowed"
        or partial.get("total_filled_making_atoms") != "40000"
        or partial.get("total_filled_taking_atoms") != "39600"
        or partial.get("remaining_making_atoms") != "60000"
        or partial.get("remaining_taking_atoms") != "59400"
        or partial.get("escrowed_remaining_making_atoms") != "60000"
    ):
        raise AssertionError("Finalized partial fill did not preserve the exact remainder: " + json.dumps(partial, sort_keys=True))
    pending_parent = call_pending(environment_name, ["status", operation_id, "--json"], env)
    if pending_parent.get("status") != "pending":
        raise AssertionError("A partial Trigger fill must leave its original escrow operation pending.")
    partial_balances = call(
        environment_name,
        [
            "read",
            "--input",
            json.dumps({"wallet": "solana-rehearsal", "network": "solana", "cluster": "mainnet-beta"}),
            "wallet.balances",
            "--json",
        ],
        env,
    )
    if (
        wallet_token_amount(partial_balances, input_mint)
        != wallet_token_amount(starting_balances, input_mint) - 100_000
        or wallet_token_amount(partial_balances, output_mint)
        != wallet_token_amount(starting_balances, output_mint) + 39_600
    ):
        raise AssertionError("Partial Trigger settlement changed wallet balances outside its exact cumulative effects.")

    fill(60_000, 59_400, close_rent_accounts=True)
    filled = read_order()
    if (
        filled.get("state") != "filled"
        or filled.get("total_filled_making_atoms") != "100000"
        or filled.get("total_filled_taking_atoms") != "99000"
        or filled.get("remaining_making_atoms") != "0"
        or filled.get("remaining_taking_atoms") != "0"
        or filled.get("escrowed_remaining_making_atoms") != "0"
        or len(filled.get("verified_fills") or []) != 2
    ):
        raise AssertionError("Finalized full fill did not close the exact cumulative quote: " + json.dumps(filled, sort_keys=True))

    # A public state read causes the observer to reconcile the original journal
    # operation from the same finalized fill history after the read path.
    final_balances = call(
        environment_name,
        [
            "read",
            "--input",
            json.dumps({"wallet": "solana-rehearsal", "network": "solana", "cluster": "mainnet-beta"}),
            "wallet.balances",
            "--json",
        ],
        env,
    )
    settled = call(environment_name, ["status", operation_id, "--json"], env)
    if settled.get("status") != "completed" or not (settled.get("effects") or {}).get("proof_digest"):
        raise AssertionError("The original Trigger operation did not settle after the full fill: " + json.dumps(settled, sort_keys=True))
    escrow_effects = [
        effect
        for effect in (settled.get("effects") or {}).get("effects", [])
        if effect.get("kind") == "escrow_progress"
    ]
    if (
        len(escrow_effects) != 1
        or escrow_effects[0].get("state") != "filled"
        or escrow_effects[0].get("input_filled") != "100000"
        or escrow_effects[0].get("input_refunded") != "0"
    ):
        raise AssertionError("The durable Trigger parent does not record exact full-fill settlement.")
    funding_effects = [
        effect
        for effect in (settled.get("effects") or {}).get("effects", [])
        if effect.get("kind") == "account_funding_progress"
    ]
    if (
        len(funding_effects) != 2
        or any(
            effect.get("state") != "closed"
            or effect.get("remaining") != "0"
            or effect.get("returned") != effect.get("funded")
            for effect in funding_effects
        )
    ):
        raise AssertionError(
            "The finalized terminal fill did not prove exact closure and refund of both funded order accounts: "
            + json.dumps(funding_effects, sort_keys=True)
        )
    create_fee_atoms = sum(
        int(effect.get("amount", {}).get("atoms", "0"))
        for effect in (created.get("effects") or {}).get("effects", [])
        if effect.get("kind") == "cost"
        and effect.get("amount", {}).get("asset", {}).get("reference") == "native"
    )
    rent_funding_atoms = sum(int(effect["funded"]) for effect in funding_effects)
    rent_return_atoms = sum(int(effect["returned"]) for effect in funding_effects)
    expected_native_after_fill = (
        int(starting_balances["native"]["amount_atoms"])
        - create_fee_atoms
        - rent_funding_atoms
        + rent_return_atoms
    )
    if int(final_balances["native"]["amount_atoms"]) != expected_native_after_fill:
        raise AssertionError(
            "Full-fill native balance did not conserve exact placement fees and finalized rent refunds: "
            + json.dumps(
                {
                    "initial_native_atoms": starting_balances["native"]["amount_atoms"],
                    "creation_fee_atoms": str(create_fee_atoms),
                    "verified_rent_funding_atoms": str(rent_funding_atoms),
                    "verified_rent_return_atoms": str(rent_return_atoms),
                    "actual_native_atoms": final_balances["native"]["amount_atoms"],
                    "expected_native_atoms": str(expected_native_after_fill),
                },
                sort_keys=True,
            )
        )
    if wallet_token_amount(final_balances, input_mint) != wallet_token_amount(starting_balances, input_mint) - 100_000:
        raise AssertionError("Trigger fill did not debit exactly its approved input from the wallet.")
    if wallet_token_amount(final_balances, output_mint) != wallet_token_amount(starting_balances, output_mint) + 99_000:
        raise AssertionError("Trigger fill did not credit exactly its verified output to the wallet.")
    reservations = _operation_reservation_count(Path(env["GOFIN_DEV_STATE_HOME"]), environment_name, operation_id)
    if reservations != 0:
        raise AssertionError("The terminal Trigger fill left durable resource reservations behind.")

    # A terminal trade does not by itself prove that the program closed its
    # auxiliary accounts. Exercise the other receipt shape: the full fill is
    # final, but funded order accounts remain open and refundable funding stays
    # represented as open rather than being credited prematurely.
    retained_input = dict(order_input)
    retained_input.update({"making_amount_atoms": "20000", "taking_amount_atoms": "19800"})
    retained_create = call(
        environment_name,
        ["execute", "jupiter.limit.create", "--input", json.dumps(retained_input),
         "--id", "solana-trigger-terminal-retained-rent", "--json"],
        env,
    )
    retained_id = retained_create.get("id")
    retained_key = retained_create.get("order_key") or (retained_create.get("preview") or {}).get("binding", {}).get("order_key")
    if retained_create.get("status") != "pending" or not isinstance(retained_id, str) or not isinstance(retained_key, str):
        raise AssertionError("The retained-account Trigger order did not enter pending escrow: " + json.dumps(retained_create, sort_keys=True))
    fill_control.write_text(
        json.dumps({"order_key": retained_key, "making_atoms": "20000", "taking_atoms": "19800"}, separators=(",", ":")),
        encoding="utf-8",
    )
    retained_status = call_pending(
        environment_name,
        ["read", "--input", json.dumps({**status_input, "order_key": retained_key}), "jupiter.limit.status", "--json"],
        env,
    )
    retained_parent = call_pending(environment_name, ["status", retained_id, "--json"], env)
    retained_funding = [
        effect for effect in (retained_parent.get("effects") or {}).get("effects", [])
        if effect.get("kind") == "account_funding_progress"
    ]
    if (
        retained_status.get("state") != "filled"
        or retained_status.get("escrowed_remaining_making_atoms") != "0"
        or retained_parent.get("status") != "completed"
        or len(retained_funding) != 2
        or any(
            effect.get("state") != "open"
            or effect.get("returned") != "0"
            or effect.get("remaining") != effect.get("funded")
            for effect in retained_funding
        )
    ):
        raise AssertionError(
            "A full fill without proved account closure must retain refundable funding: "
            + json.dumps({"status": retained_status, "parent": retained_parent}, sort_keys=True)
        )
    if _operation_reservation_count(Path(env["GOFIN_DEV_STATE_HOME"]), environment_name, retained_id) != 0:
        raise AssertionError("The retained-account full fill left durable reservations behind.")

    # Model a fill racing an explicit cancel: the finalized fill appears as the
    # order is inspected for cancellation, so only the still-open remainder is
    # returned and the parent accounts for both facts exactly once.
    race_input = dict(order_input)
    race_input.update(
        {
            "making_amount_atoms": "80000",
            "taking_amount_atoms": "79200",
            "maximum_account_funding_lamports": "5000000",
        }
    )
    race_create = call(
        environment_name,
        ["execute", "jupiter.limit.create", "--input", json.dumps(race_input), "--id", "solana-trigger-fill-cancel-race", "--json"],
        env,
    )
    if race_create.get("status") != "pending":
        raise AssertionError("The Trigger fill/cancel race order did not enter pending escrow.")
    race_id = race_create.get("id")
    race_key = race_create.get("order_key") or (race_create.get("preview") or {}).get("binding", {}).get("order_key")
    if not isinstance(race_id, str) or not isinstance(race_key, str):
        raise AssertionError("The Trigger fill/cancel race omitted its durable order identity.")
    fill_control.write_text(
        json.dumps({"order_key": race_key, "making_atoms": "25000", "taking_atoms": "24750"}, separators=(",", ":")),
        encoding="utf-8",
    )
    race_cancel = call(
        environment_name,
        [
            "execute",
            "jupiter.limit.cancel",
            "--input",
            json.dumps(
                {
                    "wallet": "solana-rehearsal",
                    "network": "solana",
                    "cluster": "mainnet-beta",
                    "order_key": race_key,
                    "original_operation_id": race_id,
                    "maximum_network_fee_lamports": "10000",
                    "maximum_account_funding_lamports": "0",
                }
            ),
            "--id",
            "solana-trigger-fill-cancel-race-cancel",
            "--json",
        ],
        env,
    )
    if race_cancel.get("status") != "completed":
        raise AssertionError("The cancel did not settle against the raced finalized partial fill: " + json.dumps(race_cancel, sort_keys=True))
    race_status = call_pending(
        environment_name,
        ["read", "--input", json.dumps({**status_input, "order_key": race_key}), "jupiter.limit.status", "--json"],
        env,
    )
    race_parent = call_pending(environment_name, ["status", race_id, "--json"], env)
    if (
        race_status.get("state") != "cancelled"
        or race_status.get("total_filled_making_atoms") != "25000"
        or race_status.get("total_filled_taking_atoms") != "24750"
        or race_status.get("remaining_making_atoms") != "55000"
        or race_status.get("remaining_taking_atoms") != "54450"
        or race_status.get("escrowed_remaining_making_atoms") != "0"
        or race_status.get("returned_making_atoms") != "55000"
        or race_parent.get("status") != "completed"
    ):
        raise AssertionError("Trigger fill/cancel race lost its exact fill, remainder, or parent settlement: " + json.dumps({"status": race_status, "parent": race_parent}, sort_keys=True))
    race_progress = [
        effect
        for effect in (race_parent.get("effects") or {}).get("effects", [])
        if effect.get("kind") == "escrow_progress"
    ]
    if (
        len(race_progress) != 1
        or race_progress[0].get("state") != "cancelled"
        or race_progress[0].get("input_filled") != "25000"
        or race_progress[0].get("input_refunded") != "55000"
    ):
        raise AssertionError("Raced cancellation did not settle the fill and returned remainder cumulatively.")

    # Expiry is a state transition, not a completed swap. Keep the quote's
    # historical remainder visible after the exact escrow refund is verified.
    expiry_input = dict(order_input)
    expiry_input.update(
        {
            "making_amount_atoms": "30000",
            "taking_amount_atoms": "29700",
            "expired_at_unix": int(time.time()) + 3,
            "maximum_account_funding_lamports": "5000000",
        }
    )
    expiry_create = call(
        environment_name,
        ["execute", "jupiter.limit.create", "--input", json.dumps(expiry_input), "--id", "solana-trigger-expiry", "--json"],
        env,
    )
    if expiry_create.get("status") != "pending":
        raise AssertionError("The expiring Trigger order did not enter pending escrow.")
    expiry_id = expiry_create.get("id")
    expiry_key = expiry_create.get("order_key") or (expiry_create.get("preview") or {}).get("binding", {}).get("order_key")
    if not isinstance(expiry_id, str) or not isinstance(expiry_key, str):
        raise AssertionError("The expiring Trigger order omitted its durable identity.")
    time.sleep(4)
    expiry_status = call_pending(
        environment_name,
        ["read", "--input", json.dumps({**status_input, "order_key": expiry_key}), "jupiter.limit.status", "--json"],
        env,
    )
    if expiry_status.get("state") != "expired_escrowed":
        raise AssertionError("The expired Trigger order was not reported as expired with escrow held: " + json.dumps(expiry_status, sort_keys=True))
    expiry_cancel = call(
        environment_name,
        [
            "execute",
            "jupiter.limit.cancel",
            "--input",
            json.dumps(
                {
                    "wallet": "solana-rehearsal",
                    "network": "solana",
                    "cluster": "mainnet-beta",
                    "order_key": expiry_key,
                    "original_operation_id": expiry_id,
                    "maximum_network_fee_lamports": "10000",
                    "maximum_account_funding_lamports": "0",
                }
            ),
            "--id",
            "solana-trigger-expiry-cancel",
            "--json",
        ],
        env,
    )
    expiry_parent = call_pending(environment_name, ["status", expiry_id, "--json"], env)
    if expiry_cancel.get("status") != "completed" or expiry_parent.get("status") != "completed":
        raise AssertionError("Expired escrow did not settle through an exact approved cancellation: " + json.dumps({"cancel": expiry_cancel, "parent": expiry_parent}, sort_keys=True))
    expiry_final = call_pending(
        environment_name,
        ["read", "--input", json.dumps({**status_input, "order_key": expiry_key}), "jupiter.limit.status", "--json"],
        env,
    )
    if (
        expiry_final.get("state") != "cancelled"
        or expiry_final.get("escrowed_remaining_making_atoms") != "0"
        or expiry_final.get("returned_making_atoms") != "30000"
    ):
        raise AssertionError("Expiry cleanup did not prove the exact returned escrow: " + json.dumps(expiry_final, sort_keys=True))
    expiry_progress = [
        effect
        for effect in (expiry_parent.get("effects") or {}).get("effects", [])
        if effect.get("kind") == "escrow_progress"
    ]
    if (
        len(expiry_progress) != 1
        or expiry_progress[0].get("state") != "cancelled"
        or expiry_progress[0].get("input_filled") != "0"
        or expiry_progress[0].get("input_refunded") != "30000"
    ):
        raise AssertionError("Expiry cancellation did not refund the exact untouched escrow.")

    before_external = call(
        environment_name,
        [
            "read",
            "--input",
            json.dumps({"wallet": "solana-rehearsal", "network": "solana", "cluster": "mainnet-beta"}),
            "wallet.balances",
            "--json",
        ],
        env,
    )
    external_control = Path(env["GOFIN_DEV_SOLANA_TRIGGER_EXTERNAL_ORDER_FILE"])
    external_control.write_text(
        json.dumps(
            {
                "input_mint": input_mint,
                "output_mint": output_mint,
                "making_atoms": "50000",
                "taking_atoms": "49000",
            },
            separators=(",", ":"),
        ),
        encoding="utf-8",
    )
    external_list = call(
        environment_name,
        [
            "read",
            "--input",
            json.dumps(
                {
                    "wallet": "solana-rehearsal",
                    "network": "solana",
                    "cluster": "mainnet-beta",
                    "order_status": "active",
                }
            ),
            "jupiter.limit.orders",
            "--json",
        ],
        env,
    )
    external_orders = external_list.get("orders") or []
    external = next(
        (
            order
            for order in external_orders
            if order.get("input_mint") == input_mint
            and order.get("raw_remaining_making_amount") == "50000"
        ),
        None,
    )
    if not external or external.get("verified_on_chain_state") != "open_escrowed":
        raise AssertionError("The externally created wallet-owned order was not discovered on chain.")
    external_cancel_input = {
        "wallet": "solana-rehearsal",
        "network": "solana",
        "cluster": "mainnet-beta",
        "order_key": external["order_key"],
        "maximum_network_fee_lamports": "10000",
        "maximum_account_funding_lamports": "0",
    }
    external_cancel = call(
        environment_name,
        [
            "execute",
            "jupiter.limit.cancel",
            "--input",
            json.dumps(external_cancel_input),
            "--id",
            "solana-trigger-external-cancel",
            "--json",
        ],
        env,
    )
    if external_cancel.get("status") != "completed" or not (external_cancel.get("effects") or {}).get("proof_digest"):
        raise AssertionError("Cancellation of the external wallet-owned order did not settle from exact proof.")
    external_status = call_pending(
        environment_name,
        [
            "read",
            "--input",
            json.dumps(
                {
                    "wallet": "solana-rehearsal",
                    "network": "solana",
                    "cluster": "mainnet-beta",
                    "order_key": external["order_key"],
                }
            ),
            "jupiter.limit.status",
            "--json",
        ],
        env,
    )
    if (
        external_status.get("state") != "cancelled"
        or external_status.get("remaining_making_atoms") != "50000"
        or external_status.get("escrowed_remaining_making_atoms") != "0"
        or external_status.get("returned_making_atoms") != "50000"
    ):
        raise AssertionError(
            "External cancellation did not distinguish returned escrow from historical remainder: "
            + json.dumps(external_status, sort_keys=True)
        )
    final_external_balances = call(
        environment_name,
        [
            "read",
            "--input",
            json.dumps({"wallet": "solana-rehearsal", "network": "solana", "cluster": "mainnet-beta"}),
            "wallet.balances",
            "--json",
        ],
        env,
    )
    if (
        wallet_token_amount(final_external_balances, input_mint)
        != wallet_token_amount(starting_balances, input_mint) - 145_000
    ):
        raise AssertionError(
            "Trigger fills and all exact escrow returns did not conserve the wallet's input balance: "
            + json.dumps(
                {
                    "starting_input_atoms": wallet_token_amount(starting_balances, input_mint),
                    "final_input_atoms": wallet_token_amount(final_external_balances, input_mint),
                    "expected_cumulative_fill_atoms": 145_000,
                },
                sort_keys=True,
            )
        )
    if wallet_token_amount(final_external_balances, output_mint) != wallet_token_amount(starting_balances, output_mint) + 143_550:
        raise AssertionError("Trigger fills did not credit exactly their cumulative verified output.")
    external_cancel_id = external_cancel.get("id")
    if isinstance(external_cancel_id, str) and _operation_reservation_count(
        Path(env["GOFIN_DEV_STATE_HOME"]), environment_name, external_cancel_id
    ) != 0:
        raise AssertionError("External-order cancellation left durable resource reservations behind.")

    return {
        "order_key": order_key,
        "operation_id": operation_id,
        "initial_order": summary(opened),
        "partial_fill": summary(partial),
        "full_fill": summary(filled),
        "settled_parent": summary(settled),
        "retained_account_full_fill": summary(retained_parent),
        "external_order": external["order_key"],
        "external_cancel": summary(external_cancel),
        "external_cancel_status": summary(external_status),
        "fill_cancel_race": summary(race_status),
        "expiry_cancel": summary(expiry_cancel),
        "input_atoms_filled": "145000",
        "output_atoms_received": "143550",
        "primary_order_input_atoms_filled": "100000",
        "primary_order_output_atoms_received": "99000",
        "primary_order_rent_returned_lamports": str(rent_return_atoms),
        "final_input_atoms": wallet_token_amount(final_external_balances, input_mint),
        "final_output_atoms": wallet_token_amount(final_external_balances, output_mint),
        "native_lamports_after_order_funding": final_external_balances["native"]["amount_atoms"],
        "external_cancel_returned_input_atoms": external_status["returned_making_atoms"],
        "race_returned_input_atoms": race_status["returned_making_atoms"],
        "expiry_returned_input_atoms": expiry_final["returned_making_atoms"],
        "synthetic_only": True,
    }


def _operation_reservation_count(state_home: Path, environment_name: str, operation_id: str) -> int:
    """Read the exact selected development environment's durable journal."""
    for metadata_path in state_home.rglob("environment.json"):
        try:
            metadata = json.loads(metadata_path.read_text(encoding="utf-8"))
            if metadata.get("name") != environment_name:
                continue
            database = Path(metadata["database"]).resolve(strict=True)
            with sqlite3.connect(f"{database.as_uri()}?mode=ro", uri=True, timeout=5) as connection:
                row = connection.execute(
                    "SELECT COUNT(*) FROM reservations WHERE operation_id = ?", (operation_id,)
                ).fetchone()
                return int(row[0])
        except (OSError, KeyError, sqlite3.Error, ValueError, TypeError):
            continue
    raise AssertionError("Could not resolve the selected synthetic environment's operation journal.")
