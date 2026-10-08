#!/usr/bin/env python3
"""Focused synthetic checks for the isolated Anvil fork runner."""
from __future__ import annotations

import importlib.util
import json
from pathlib import Path
import sys
import tempfile
import threading
import unittest
from unittest import mock

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT / "scripts"))
SPEC = importlib.util.spec_from_file_location("verify_rust_fork", ROOT / "scripts/verify-rust-fork.py")
assert SPEC and SPEC.loader
runner = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(runner)


def metadata(instance: str) -> dict[str, object]:
    return {
        "clientVersion": "anvil/v1.6.0",
        "chainId": 1,
        "instanceId": "0x" + instance * 64,
        "forkedNetwork": {
            "forkBlockNumber": runner.FORK_BLOCK,
            "forkBlockHash": runner.FORK_HASH,
        },
    }


class FakeProcess:
    def __init__(self, status: int | None):
        self.returncode = status
        self.pid = id(self)

    def poll(self) -> int | None:
        return self.returncode


class FakeOwned:
    def __init__(self, role: str, status: int | None, events: list[str], marker: dict[str, object]):
        self.role = role
        self.process = FakeProcess(status)
        self.marker = marker
        self.events = events

    def stop(self) -> bool:
        self.events.append("stop:" + self.role)
        return True

    def detach(self) -> None:
        self.events.append("retain:" + self.role)


class Lease:
    def __init__(self, port: int):
        self.port = port
        self.url = f"http://127.0.0.1:{port}"

    def release(self) -> None:
        pass


class ForkRunnerTests(unittest.TestCase):
    def test_unresolved_test_group_retains_node_without_reset(self) -> None:
        events = []

        def start(command, *, log_path, marker_path, **kwargs):
            role = "anvil" if "--fork-url" in command else "cargo"
            log_path.write_text("synthetic log\n")
            owned = FakeOwned(role, None if role == "anvil" else 0, events, {})
            if role == "cargo":
                owned.stop = lambda: False
            return owned

        with tempfile.TemporaryDirectory() as temporary:
            with mock.patch.object(runner.subprocess, "run", return_value=mock.Mock(returncode=0)), \
                 mock.patch.object(runner, "allocate_loopback", return_value=Lease(42000)), \
                 mock.patch.object(runner, "start_owned_process", side_effect=start), \
                 mock.patch.object(runner, "wait_ready", return_value=metadata("a")):
                self.assertEqual(runner.verify(Path(temporary), Path("/fake/anvil")), 1)
        self.assertEqual(events, ["retain:anvil"])

    def test_endpoint_metadata_is_strict_and_instance_id_is_required(self) -> None:
        self.assertTrue(runner.valid_metadata(metadata("a")))
        for changed in (
            {**metadata("a"), "chainId": 5},
            {**metadata("a"), "forkedNetwork": None},
            {**metadata("a"), "instanceId": "0xabc"},
        ):
            self.assertFalse(runner.valid_metadata(changed))

    def test_readiness_requires_owned_listener_and_live_child(self) -> None:
        owned = FakeOwned("anvil", None, [], {"pid": 7})
        with mock.patch.object(runner, "READY_ATTEMPTS", 1), mock.patch.object(
            runner, "process_matches", return_value=True
        ), mock.patch.object(runner, "child_owns_listener", return_value=False), mock.patch.object(
            runner, "rpc"
        ) as rpc, mock.patch.object(runner.time, "sleep"):
            self.assertIsNone(runner.wait_ready("http://127.0.0.1:8546", owned))
            rpc.assert_not_called()

    def test_real_owned_listener_waits_for_late_start_and_is_cleaned_up(self) -> None:
        from fin_dev_resources import start_owned_process

        with tempfile.TemporaryDirectory() as temporary:
            work = Path(temporary)
            lease = runner.allocate_loopback()
            endpoint, port = lease.url, lease.port
            lease.release()
            result = metadata("c")
            child = (
                "import http.server,json,time;"
                "time.sleep(0.35);"
                f"data={json.dumps(result)!r};"
                "class_handler=type('Handler',(http.server.BaseHTTPRequestHandler,),{"
                "'do_POST':lambda self:(self.rfile.read(int(self.headers['Content-Length'])),"
                "self.send_response(200),self.send_header('Content-Type','application/json'),"
                "self.end_headers(),self.wfile.write(json.dumps({'jsonrpc':'2.0','id':1,'result':json.loads(data)}).encode())),"
                "'log_message':lambda *args:None});"
                f"http.server.HTTPServer(('127.0.0.1',{port}),class_handler).serve_forever()"
            )
            owned = start_owned_process(
                [sys.executable, "-u", "-c", child],
                log_path=work / "fixture.log",
                marker_path=work / "fixture.owner.json",
            )
            try:
                with mock.patch.object(runner, "READY_ATTEMPTS", 4):
                    observed = runner.wait_ready(endpoint, owned)
                self.assertEqual(observed, result)
                self.assertTrue(runner.child_owns_listener(owned.process.pid, port))
            finally:
                self.assertTrue(owned.stop())
            self.assertIsNotNone(owned.process.poll())

    def test_two_invocations_get_distinct_ports_instances_and_owned_cleanup(self) -> None:
        port_lock = threading.Lock()
        next_port = 42000
        event_lock = threading.Lock()
        all_events: list[str] = []
        observed_env: list[dict[str, str]] = []
        thread_errors: list[BaseException] = []

        def lease() -> Lease:
            nonlocal next_port
            with port_lock:
                value = next_port
                next_port += 1
            return Lease(value)

        def start(command: list[str], *, log_path: Path, marker_path: Path, env=None, **_kwargs):
            role = "anvil" if "--fork-url" in command else "cargo"
            log_path.parent.mkdir(parents=True, exist_ok=True)
            log_path.write_text("synthetic test log\n")
            marker = {"pid": 10 if role == "anvil" else 11, "started": role}
            with event_lock:
                all_events.append("start:" + role)
                if role == "cargo":
                    observed_env.append(dict(env or {}))
            status = None if role == "anvil" else 0
            return FakeOwned(role, status, all_events, marker)

        def wait_ready(endpoint: str, _owned):
            instance_char = "a" if endpoint.endswith(":42000") else "b"
            return metadata(instance_char)

        def run_one(index: int) -> None:
            try:
                with tempfile.TemporaryDirectory() as temporary:
                    source = Path(temporary)
                    (source / "work").mkdir()
                    self.assertEqual(runner.verify(source, Path("/fake/anvil")), 0)
            except BaseException as error:
                thread_errors.append(error)

        threads = [threading.Thread(target=run_one, args=(i,)) for i in range(2)]
        with mock.patch.object(runner.subprocess, "run", return_value=mock.Mock(returncode=0)), mock.patch.object(
            runner, "allocate_loopback", side_effect=lease
        ), mock.patch.object(
            runner, "start_owned_process", side_effect=start
        ), mock.patch.object(runner, "wait_ready", side_effect=wait_ready):
            for thread in threads:
                thread.start()
            for thread in threads:
                thread.join()
        self.assertEqual(thread_errors, [])
        urls = {entry["FIN_FORK_RPC_URL"] for entry in observed_env}
        identities = {entry["FIN_FORK_INSTANCE_ID"] for entry in observed_env}
        self.assertEqual(urls, {"http://127.0.0.1:42000", "http://127.0.0.1:42001"})
        self.assertEqual(len(identities), 2)
        self.assertTrue(all(event.endswith("anvil") or event.endswith("cargo") for event in all_events))
        for index, event in enumerate(all_events):
            if event == "stop:anvil":
                self.assertIn("stop:cargo", all_events[:index])

    def test_occupied_port_startup_failure_retries_with_bounded_attempts(self) -> None:
        events: list[str] = []
        ports = iter(range(43000, 43008))
        launches = 0

        def start(command: list[str], *, log_path: Path, marker_path: Path, **_kwargs):
            nonlocal launches
            launches += 1
            return FakeOwned("anvil", 1, events, {"pid": 20 + launches})

        with tempfile.TemporaryDirectory() as temporary:
            source = Path(temporary)
            (source / "work").mkdir()
            with mock.patch.object(runner.subprocess, "run", return_value=mock.Mock(returncode=0)), mock.patch.object(
                runner, "allocate_loopback", side_effect=lambda: Lease(next(ports))
            ), mock.patch.object(runner, "start_owned_process", side_effect=start), mock.patch.object(
                runner, "wait_ready", return_value=None
            ):
                self.assertEqual(runner.verify(source, Path("/fake/anvil")), 1)
        self.assertEqual(launches, runner.ALLOCATE_ATTEMPTS)
        self.assertEqual(events, ["stop:anvil"] * runner.ALLOCATE_ATTEMPTS)

    def test_archive_start_failure_keeps_owned_live_node_log_and_stops_once(self) -> None:
        events: list[str] = []
        with tempfile.TemporaryDirectory() as temporary:
            source = Path(temporary)
            (source / "work").mkdir()
            live = FakeOwned("anvil", None, events, {"pid": 70})
            with mock.patch.object(runner.subprocess, "run", return_value=mock.Mock(returncode=0)), mock.patch.object(
                runner, "allocate_loopback", return_value=Lease(44000)
            ), mock.patch.object(runner, "start_owned_process", return_value=live), mock.patch.object(
                runner, "wait_ready", return_value=None
            ):
                self.assertEqual(runner.verify(source, Path("/fake/anvil")), 1)
        self.assertEqual(events, ["stop:anvil"])

    def test_cancellation_during_readiness_cleans_owned_node(self) -> None:
        events: list[str] = []
        with tempfile.TemporaryDirectory() as temporary:
            source = Path(temporary)
            (source / "work").mkdir()
            owned = FakeOwned("anvil", None, events, {"pid": 99})
            with mock.patch.object(runner.subprocess, "run", return_value=mock.Mock(returncode=0)), mock.patch.object(
                runner, "allocate_loopback", return_value=Lease(45000)
            ), mock.patch.object(runner, "start_owned_process", return_value=owned), mock.patch.object(
                runner, "wait_ready", side_effect=KeyboardInterrupt
            ):
                with self.assertRaises(KeyboardInterrupt):
                    runner.verify(source, Path("/fake/anvil"))
        self.assertEqual(events, ["stop:anvil"])


if __name__ == "__main__":
    unittest.main(verbosity=2)
