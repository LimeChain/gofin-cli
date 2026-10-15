#!/usr/bin/env python3
"""Explicit live read audit through an installed GoFin broker, never credential files.

Private account results are retained only when --report names a new private file.
No writes, previews, approvals, invented order IDs or unbounded history scans.
"""
import argparse
from datetime import datetime, timezone
from decimal import Decimal
import json
import os
from pathlib import Path
import subprocess
import sys


PUBLIC = {"binance.spot.markets", "binance.futures.markets"}
ACCOUNT_READS = {
    "binance.account-identity", "binance.balances", "binance.permissions",
    "binance.networks", "binance.deposit-address", "binance.deposit-addresses",
    "binance.deposit-history", "binance.withdrawal-history", "binance.whitelist",
    "binance.hierarchy", "binance.futures.balances", "binance.futures.positions",
}
TRADING = {
    "binance.spot.orders", "binance.spot.trades", "binance.spot.order",
    "binance.futures.orders", "binance.futures.trades", "binance.futures.income",
    "binance.futures.order",
}


def call(cli, arguments):
    completed = subprocess.run([cli, *arguments, "--json"], capture_output=True,
                               text=True, timeout=120)
    value = json.loads(completed.stdout)
    return completed.returncode, value


def audit(cli, names, symbol, coin, network):
    _, capabilities = call(cli, ["capabilities"])
    reads = {row["name"] for row in capabilities["operations"]
             if row["name"].startswith("binance.") and row["kind"] == "read"}
    if reads != PUBLIC | ACCOUNT_READS | TRADING:
        raise ValueError("Binance read inventory changed; update audit recipes before running.")
    _, connections = call(cli, ["connections", "list"])
    connections = {row["name"]: row for row in connections if row["provider"] == "binance"}
    if any(name not in connections for name in names):
        raise ValueError("Every selected account must be an enrolled Binance connection.")
    records = []

    def read(operation, args, label):
        # Every dispatch is checked against offline read-kind discovery.
        if operation not in reads:
            raise ValueError("The audit refuses operations outside the Binance read inventory.")
        try:
            status, result = call(cli, ["read", operation, "--input", json.dumps(args)])
            ok = status == 0 and not result.get("error")
            row = {"account": label, "operation": operation, "input": args,
                   "status": "passed" if ok else "failed", "result": result}
        except (subprocess.TimeoutExpired, json.JSONDecodeError, OSError) as failure:
            row = {"account": label, "operation": operation, "input": args,
                   "status": "failed", "error": type(failure).__name__}
            ok, result = False, None
        records.append(row)
        code = (row.get("result") or {}).get("error", {}).get("code", row.get("error", ""))
        print(f"{label}: {operation}: {row['status']} {code}".rstrip(), flush=True)
        return result if ok else None

    for operation in sorted(PUBLIC):
        read(operation, {}, "public")
    for name in names:
        account = {"account": name}
        for operation in sorted(ACCOUNT_READS - {"binance.hierarchy", "binance.futures.positions"}):
            args = dict(account)
            if operation == "binance.networks":
                args["coin"] = coin
            if operation in {"binance.deposit-address", "binance.deposit-addresses"}:
                args.update(coin=coin, network=network)
            read(operation, args, name)
        authority = name if connections[name]["role"] == "main" else connections[name]["parent"]
        read("binance.hierarchy", {"authority": authority}, name)
        positions = read("binance.futures.positions", account, name)
        futures_symbols = [symbol]
        if positions:
            for position in positions["positions"]:
                if Decimal(position["positionAmt"]) != 0 and position["symbol"] not in futures_symbols:
                    futures_symbols.append(position["symbol"])
        for market in ["spot", "futures"]:
            # Each operation is checked; histories remain explicitly bounded.
            symbols = [symbol] if market == "spot" else futures_symbols[:4]
            candidates = []
            for selected in symbols:
                args = {"account": name, "symbol": selected, "limit": 100}
                orders = read(f"binance.{market}.orders", args, name)
                trades = read(f"binance.{market}.trades", args, name)
                if market == "futures":
                    read("binance.futures.income", args, name)
                for result, key in [(orders, "orders"), (trades, "trades")]:
                    if result and result.get(key):
                        candidates.append((selected, result[key][-1]["orderId"]))
            if candidates:
                selected, order_id = candidates[0]
                read(f"binance.{market}.order", {"account": name, "symbol": selected,
                     "order_id": order_id}, name)
            else:
                operation = f"binance.{market}.order"
                records.append({"account": name, "operation": operation, "status": "not_run",
                                "reason": "No existing order ID observed in the bounded selected markets."})
                print(f"{name}: {operation}: not_run (no observed order ID)", flush=True)
    return {"observed_at": datetime.now(timezone.utc).isoformat(),
            "read_only": True, "history_limit": 100, "futures_symbol_limit": 4,
            "records": records}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cli", default="/Library/Fin/runtime/gofin")
    parser.add_argument("--accounts", nargs="+", required=True)
    parser.add_argument("--symbol", default="ETHUSDT")
    parser.add_argument("--coin", default="ETH")
    parser.add_argument("--network", default="ETH")
    parser.add_argument("--report", type=Path, help="New private report file; never commit its contents.")
    args = parser.parse_args()
    # Refuse overwrite before making any calls; create with private permissions.
    report = None
    if args.report:
        report = os.fdopen(os.open(args.report, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600), "w")
    try:
        result = audit(args.cli, args.accounts, args.symbol, args.coin, args.network)
        if report:
            json.dump(result, report, indent=2)
            report.write("\n")
        return 1 if any(row["status"] == "failed" for row in result["records"]) else 0
    finally:
        if report:
            report.close()


if __name__ == "__main__":
    sys.exit(main())
