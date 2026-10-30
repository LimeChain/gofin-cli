#!/usr/bin/env python3
"""Generate, persist and remove EVM/Solana wallets through an isolated dev CLI/broker."""
import argparse
import datetime
import errno
import json
import os
from pathlib import Path
import pty
import re
import select
import shlex
import subprocess
import time

ROOT = Path(__file__).resolve().parents[2]
ANSI = re.compile(r"\x1b\[[0-9;?]*[A-Za-z]")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--mode", choices=("synthetic", "live"), default="synthetic")
    args = parser.parse_args()
    stamp = datetime.datetime.now(datetime.timezone.utc).strftime("%Y%m%dT%H%M%S.%fZ")
    evidence = ROOT / ".context" / "test-runs" / f"wallet-generation-{args.mode}-{stamp}"
    evidence.mkdir(parents=True)
    env = dict(os.environ, GOFIN_DEV_STATE_HOME=str(evidence / "state"))
    env.pop("NO_COLOR", None)
    env["TERM"] = "xterm-256color"
    name = "wallet-generation"
    calls = []
    report = {"mode": args.mode, "authorization": "automatic development approval",
              "financial_writes": "not run; no financial movement required", "costs": "0",
              "native_protected_approval": "not run", "status": "running", "calls": calls}

    def run(target, arguments=None, tty=False, extra_env=None, expected=0):
        command = ["make", target, f"DEV_ENV={name}"]
        if arguments is not None:
            command.append("ARGS=" + shlex.join(arguments))
        else:
            command += [f"DEV_MODE={args.mode}", "DEV_FLAGS=--json"]
        child_env = dict(env, **(extra_env or {}))
        if tty:
            master, slave = pty.openpty()
            process = subprocess.Popen(command, cwd=ROOT, env=child_env, stdin=slave,
                                       stdout=slave, stderr=slave, start_new_session=True)
            os.close(slave)
            data = bytearray()
            deadline = time.monotonic() + 120
            try:
                while True:
                    if time.monotonic() > deadline:
                        process.kill()
                        raise AssertionError("CLI did not finish within 120 seconds")
                    ready, _, _ = select.select([master], [], [], 0.2)
                    if ready:
                        try:
                            chunk = os.read(master, 65536)
                        except OSError as exc:
                            if exc.errno == errno.EIO:
                                break
                            raise
                        if not chunk:
                            break
                        data.extend(chunk)
                    elif process.poll() is not None:
                        break
                code = process.wait(timeout=5)
            finally:
                os.close(master)
            output = data.decode("utf-8", errors="replace")
        else:
            result = subprocess.run(command, cwd=ROOT, env=child_env, stdin=subprocess.DEVNULL,
                                    capture_output=True, text=True, timeout=600)
            code, output = result.returncode, result.stdout + result.stderr
        filename = f"{len(calls):02d}-{target}.log"
        (evidence / filename).write_text(output)
        calls.append({"command": command, "exit": code, "log": filename})
        assert (code == 0) == (expected == 0), f"Unexpected exit {code}; see {evidence / filename}"
        return output

    def cli(arguments, **kwargs):
        return run("dev-env-cli", arguments, **kwargs)

    def listing():
        return json.loads(cli(["connections", "--json"]))

    def identities(value):
        found = {}
        if isinstance(value, dict):
            if isinstance(value.get("name"), str) and isinstance(value.get("address"), str):
                found[value["name"]] = value["address"]
            for child in value.values():
                found.update(identities(child))
        elif isinstance(value, list):
            for child in value:
                found.update(identities(child))
        return found

    try:
        startup = json.loads(run("dev-env-start"))
        assert startup["mode"] == args.mode
        report["build_id"] = startup["buildId"]
        report["git_commit"] = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip()
        report["diff_sha256"] = __import__("hashlib").sha256(subprocess.check_output(["git", "diff", "HEAD"], cwd=ROOT)).hexdigest()
        cli(["capabilities", "--json"])
        cli(["operations", "--json"])
        cli(["policy", "show", "--json"])
        assert not identities(listing())
        addresses = {}
        for network in ("evm", "solana"):
            wallet = "test-" + network
            output = cli(["connections", "generate", network, wallet], tty=True,
                         extra_env={"NO_COLOR": "1"} if network == "solana" else {})
            plain = ANSI.sub("", output)
            address = re.search(r"Address: (\S+)", plain)
            assert address, "Generated address missing from human result"
            addresses[wallet] = address.group(1)
            assert f'wallet "{wallet}" generated.' in plain
            assert "Generating and saving wallet" in plain
            assert re.search(r"\r {8,}\r", output), "Progress row must clear before result"
            assert "\x1b[32m" in output if network == "evm" else "\x1b[32m" not in output
            assert "automatic approval" in plain
            assert identities(listing())[wallet] == addresses[wallet]
            duplicate = cli(["connections", "generate", network, "--name", wallet, "--json"], expected=1)
            assert '"error"' in duplicate
            assert identities(listing())[wallet] == addresses[wallet]
        run("dev-env-stop")
        run("dev-env-start")
        assert identities(listing()) == addresses, "Wallet identities changed after restart"
        for wallet in addresses:
            output = cli(["connections", "remove", "--name", wallet], tty=True,
                         extra_env={"TERM": "dumb"})
            assert f'Connection "{wallet}" removed.' in output
            assert "\x1b" not in output, "TERM=dumb must be plain"
            assert wallet not in identities(listing())
        run("dev-env-stop")
        run("dev-env-start")
        assert not identities(listing()), "Removed wallets returned after restart"
        report.update(status="passed", addresses=addresses,
                      assertions=["human public address", "TTY progress and semantic color", "NO_COLOR and TERM=dumb",
                                  "duplicate rejection preserves identity", "restart persistence", "same connections removed", "removal survives restart"])
    except BaseException as exc:
        report.update(status="failed", error=str(exc))
        raise
    finally:
        try:
            run("dev-env-stop")
        finally:
            (evidence / "report.json").write_text(json.dumps(report, indent=2) + "\n")
            print(f"{report['status'].upper()}: {evidence / 'report.json'}")


if __name__ == "__main__":
    main()
