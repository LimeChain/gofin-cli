#!/usr/bin/env python3
"""Safety contracts for local-only finalization orchestration."""
import base64
import importlib.util
from pathlib import Path
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location('native', ROOT/'scripts/e2e/surfpool-native.py')
native = importlib.util.module_from_spec(spec)
spec.loader.exec_module(native)


class FinalizationTests(unittest.TestCase):
    url = 'http://127.0.0.1:8899'

    def clock(self, timestamp):
        data = bytes(32) + timestamp.to_bytes(8, 'little', signed=True)
        return {'value': {'data': [base64.b64encode(data).decode(), 'base64']}}

    def test_already_satisfied_does_not_move_clock(self):
        with patch.object(native, 'local_surfpool_rpc', return_value=101) as rpc:
            evidence = native.advance_finalized_slot(self.url, 100)
        self.assertEqual(rpc.call_count, 1)
        self.assertEqual(evidence['targets'], [])

    def test_success_response_without_finalized_evidence_times_out(self):
        def rpc(url, method, params, timeout):
            if method == 'surfnet_timeTravel': return {'absoluteSlot': 163}
            if method == 'getAccountInfo': return self.clock(1000)
            return 100 if params[0]['commitment'] == 'finalized' else 131
        with patch.object(native, 'local_surfpool_rpc', side_effect=rpc):
            with self.assertRaises(TimeoutError):
                native.advance_finalized_slot(self.url, 100, timeout=0.01)

    def test_forward_target_and_verified_timestamp(self):
        calls=[]; jumped=False
        def rpc(url, method, params, timeout):
            nonlocal jumped
            calls.append((method,params))
            if method == 'surfnet_timeTravel': jumped=True; return {}
            if method == 'getAccountInfo': return self.clock(1003 if jumped else 1000)
            return (132 if jumped else 100) if params[0]['commitment']=='finalized' else (163 if jumped else 131)
        with patch.object(native, 'local_surfpool_rpc', side_effect=rpc):
            evidence=native.advance_finalized_slot(self.url,100)
        self.assertEqual(evidence['targets'],[163])
        self.assertGreater(evidence['finalized_slot'],100)
        self.assertEqual(evidence['simulated_timestamp_after'],1003)

    def test_processed_or_timestamp_regression_fails(self):
        for regression in ('slot','timestamp'):
            with self.subTest(regression=regression):
                jumped=False
                def rpc(url,method,params,timeout):
                    nonlocal jumped
                    if method=='surfnet_timeTravel': jumped=True;return {}
                    if method=='getAccountInfo':return self.clock(999 if jumped else 1000)
                    if params[0]['commitment']=='finalized':return 132 if jumped else 100
                    return (130 if regression=='slot' else 163) if jumped else 131
                with patch.object(native,'local_surfpool_rpc',side_effect=rpc):
                    with self.assertRaisesRegex(RuntimeError,'regressed'):
                        native.advance_finalized_slot(self.url,100)

    def test_overtaken_target_rechecks_actual_finalization(self):
        jumped=False; calls=[]
        def rpc(url,method,params,timeout):
            nonlocal jumped
            calls.append(method)
            if method=='surfnet_timeTravel':
                jumped=True
                raise native.LocalSurfpoolRpcError({'code':-32603,'message':'Internal error','data':'Cannot travel to past slot: target=163, current=164'})
            if method=='getAccountInfo':return self.clock(1000)
            return (133 if jumped else 100) if params[0]['commitment']=='finalized' else 131
        with patch.object(native,'local_surfpool_rpc',side_effect=rpc):
            evidence=native.advance_finalized_slot(self.url,100)
        self.assertEqual(evidence['past_slot_races'],1)
        self.assertEqual(calls.count('surfnet_timeTravel'),1)
        self.assertGreater(evidence['finalized_slot'],100)

    def test_unrecognized_error_is_not_hidden(self):
        def rpc(url,method,params,timeout):
            if method=='surfnet_timeTravel':raise native.LocalSurfpoolRpcError({'message':'clock unavailable'})
            if method=='getAccountInfo':return self.clock(1000)
            return 100 if params[0]['commitment']=='finalized' else 131
        with patch.object(native,'local_surfpool_rpc',side_effect=rpc):
            with self.assertRaises(native.LocalSurfpoolRpcError):
                native.advance_finalized_slot(self.url,100)

    def test_remote_ambiguous_and_redirecting_rpc_are_rejected(self):
        with patch.object(native.urllib.request,'build_opener') as opener:
            for url in ('https://api.mainnet-beta.solana.com','http://localhost:8899',
                        'http://127.0.0.1.example.com:8899','http://127.0.0.1:8899/?remote=x',
                        'http://user@127.0.0.1:8899','http://127.0.0.1'):
                with self.subTest(url=url),self.assertRaises(ValueError):
                    native.advance_finalized_slot(url,100)
            opener.assert_not_called()
        with self.assertRaises(RuntimeError):
            native.NoRpcRedirect().redirect_request(None,None,None,None,None,None)


if __name__=='__main__':unittest.main(verbosity=2)
