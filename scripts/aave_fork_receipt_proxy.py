"""Owned-loopback Anvil receipt model, never production fee evidence.

Anvil OP deducts extra fees without exposing OP receipt fields. Attribute the
exact remaining local native debit as a single modeled fee. Nitro runs as a
local EVM with zero modeled L1 gas. This mirrors the Rust owned-fork fixture.
"""
import json
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from threading import Thread, Lock


class ReceiptProxy:
    def __init__(self, rehearsal, raw_endpoint, chain, weth):
        self.rehearsal, self.raw_endpoint, self.chain, self.weth = rehearsal, raw_endpoint, chain, weth
        self.server = None
        self.metrics_lock = Lock()
        # Anvil lazily loads fork state; concurrent historical eth_call requests
        # can stall its state cache. Serialize only this owned test transport.
        self.request_lock = Lock()

    def raw(self, method, params):
        return self.rehearsal.rpc_raw(self.raw_endpoint, method, params)

    def request(self, method, params):
        meta = self.raw('anvil_metadata', [])
        assert meta['instanceId'] == self.rehearsal.identity['instanceId']
        assert meta['forkedNetwork'] == self.rehearsal.identity['forkedNetwork']
        if self.chain in (8453,42161) and method == 'eth_maxPriorityFeePerGas':
            return hex(10**9)  # Deterministic local tip; no production bidder change.
        if self.chain == 8453 and method == 'eth_call' and params[0].get('to','').lower() in ['0x420000000000000000000000000000000000000f','0x4200000000000000000000000000000000000015']:
            data = params[0].get('data','')
            if data.startswith('0xf1c7a58b'):
                value = int(self.raw(method, params),16)+10**12  # Conservative modeled OP component envelope.
            else:
                value = 1 if data.startswith('0x960e3a23') else 0
            return '0x'+format(value,'064x')
        result = self.raw(method, params)
        if self.chain == 8453 and method == 'eth_getBlockByNumber' and isinstance(result,dict):
            result['extraData']='0x'+'00'*9
        if self.chain == 42161 and method == 'eth_getTransactionReceipt' and isinstance(result,dict):
            result['gasUsedForL1']='0x0'
        if self.chain == 8453 and method == 'eth_getTransactionReceipt' and isinstance(result,dict):
            number=int(result['blockNumber'],16)
            block=self.raw('eth_getBlockByNumber',[result['blockNumber'],True])
            assert len(block['transactions'])==1 and block['transactions'][0]['hash']==result['transactionHash']
            tx=block['transactions'][0];owner=tx['from'].lower()
            allowed_owners = {self.rehearsal.owner.lower()}
            if getattr(self.rehearsal, 'secondary', None) is not None:
                allowed_owners.add(self.rehearsal.secondary.lower())
            assert owner in allowed_owners
            before=int(self.raw('eth_getBalance',[owner,hex(number-1)]),16);after=int(self.raw('eth_getBalance',[owner,hex(number)]),16)
            withdrawal='0x7fcf532c15f0a6db0bd6d0e038bea71d30d808c7d98cb3bf7268a95bf5081b65'
            inflow=0
            for log in result['logs']:
                if log['address'].lower()==self.weth and log['topics'][0]==withdrawal:
                    assert log['topics'][1]=='0x'+owner[2:].rjust(64,'0')
                    inflow+=int(log['data'],16)
            execution=int(result['gasUsed'],16)*int(result['effectiveGasPrice'],16)
            extras=before+inflow-after-int(tx['value'],16)-execution;assert extras>=0
            result.update(l1Fee=hex(extras),operatorFeeScalar='0x0',operatorFeeConstant='0x0')
        return result

    def start(self):
        proxy=self
        class Handler(BaseHTTPRequestHandler):
            def do_POST(self):
                request={}
                started=time.monotonic()
                try:
                    request=json.loads(self.rfile.read(int(self.headers['Content-Length'])))
                    with proxy.request_lock:
                        value={'jsonrpc':'2.0','id':request['id'],'result':proxy.request(request['method'],request.get('params', []))}
                except Exception as error:
                    value={'jsonrpc':'2.0','id':request.get('id'),'error':{'code':-32000,'message':str(error)}}
                params=request.get('params',[])
                meta={'method':request.get('method'),'seconds':round(time.monotonic()-started,3),'error':value.get('error')}
                if meta['method']=='eth_call':meta.update(target=params[0].get('to'),selector=params[0].get('data','')[:10],block=params[1])
                with proxy.metrics_lock:
                    with (proxy.rehearsal.directory/'transport-timing.jsonl').open('a') as log:log.write(json.dumps(meta)+'\n')
                payload=json.dumps(value).encode();self.send_response(200);self.send_header('Content-Type','application/json');self.send_header('Content-Length',str(len(payload)));self.end_headers();self.wfile.write(payload)
            def log_message(self,*args):pass
        self.server=ThreadingHTTPServer(('127.0.0.1',0),Handler)
        Thread(target=self.server.serve_forever,daemon=True).start()
        self.rehearsal.save('fee-model',{'chain_id':self.chain,'production_writes':0,'evidence':'Modeled local receipt components and conservative extra fee bound; exact Anvil cash verified. No authentic rollup fee evidence.'})
        return 'http://127.0.0.1:'+str(self.server.server_port)

    def stop(self):
        if self.server:self.server.shutdown();self.server.server_close()
