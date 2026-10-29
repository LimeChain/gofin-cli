#!/usr/bin/env python3
"""Positive JLP lifecycle using public CLI, cloned program and owned Surfpool."""
import argparse
import base64
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import time
import uuid
import tempfile
import threading
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ROOT = Path(__file__).resolve().parents[2]
PROGRAM = 'PERPHjGBqRHArX4DySjwM6UJHiR3sWAatqfdBS2qQJu'
JLP = '27G8MtK7VtTcCHkpASjSDdkWWYfoqT6ggEuKidVJidD4'
TOKEN = 'TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA'
USDC = 'EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v'

def load(path, name):
    spec = importlib.util.spec_from_file_location(name, ROOT / path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module

def discover(helper, rpc):
    """Decode dependency identities only; Surfpool owns every upstream fetch."""
    def account(address, owner=None):
        value = rpc('getMultipleAccounts', [[address], {'encoding':'base64'}])['value'][0]
        if value is None or (owner is not None and value['owner'] != owner):
            raise RuntimeError(f'Missing or unexpected owner for JLP dependency {address}')
        return helper.account_data(value, address)
    pool = helper.derive_address(PROGRAM, 'string:pool', 'string:Pool')
    custody = helper.derive_address(PROGRAM, 'string:custody', f'pubkey:{pool}', f'pubkey:{USDC}')
    custody_token = helper.account_pubkey(account(custody, PROGRAM), 72)
    pool_data = account(pool, PROGRAM)
    name_length = int.from_bytes(pool_data[8:12], 'little')
    offset = 12 + name_length
    count = int.from_bytes(pool_data[offset:offset+4], 'little')
    if not 0 < count <= 32 or offset + 4 + 32 * count > len(pool_data):
        raise RuntimeError('JLP pool custody layout is invalid')
    custodies = [helper.account_pubkey(pool_data, offset+4+32*i) for i in range(count)]
    if custody not in custodies:
        raise RuntimeError('Selected USDC custody is not part of the JLP pool')
    oracles = [helper.account_pubkey(account(address, PROGRAM), 384) for address in custodies]
    assets = []
    for mint in (USDC, JLP):
        data = account(mint, TOKEN)
        if len(data) != 82 or data[45] != 1:
            raise RuntimeError(f'Invalid initialized SPL mint {mint}')
        assets.append({'mint':mint, 'token_program':TOKEN, 'decimals':data[44]})
    addresses = sorted(set([PROGRAM, pool, custody, custody_token, TOKEN, helper.ATA,
                            *custodies, *oracles, *(asset['mint'] for asset in assets)]))
    if len(addresses) > 100:
        raise RuntimeError('JLP dependency preload exceeds one native account batch')
    return {'accounts':addresses, 'oracles':oracles, 'assets':assets}


def main():
    parser=argparse.ArgumentParser()
    parser.add_argument('--summary',type=Path)
    args=parser.parse_args()
    shared = load('scripts/test-meteora-cli-validator.py', 'meteora_cli')
    native = load('scripts/e2e/surfpool-native.py', 'surfpool_native')
    shared.validate_surfpool()
    helper = shared.load_validator_helpers()
    shared.private_directory(shared.EVIDENCE)
    # This root is enforced by the broker's synthetic-key confinement check.
    evidence = Path(tempfile.mkdtemp(prefix='jlp-native-', dir=shared.EVIDENCE))
    run_id = uuid.uuid4().hex[:12]
    with native.Surfpool(evidence / 'discovery') as discovery:
        manifest = discover(helper, discovery.rpc)
    snapshot_path = evidence / 'dependencies.json'
    native.save(snapshot_path, {address:None for address in manifest['accounts']})
    with native.Surfpool(evidence / 'execution', snapshot_path) as node:
        loaded = native.validate_preload(node.rpc, manifest['accounts'],
            [PROGRAM, TOKEN, helper.ATA], evidence / 'preload-evidence.json')
        if discover(helper, native.cached_rpc(loaded)) != manifest:
            raise RuntimeError('JLP dependency graph changed between discovery and execution preload')
        wallet, key = native.wallet(node.rpc, evidence, 'wallet', 1_000_000_000_000)
        balances = [native.fund_token(node.rpc, wallet, asset,
                    1_000_000_000_000_000 if asset['mint'] == USDC else 0)
                    for asset in manifest['assets']]
        native.save(evidence / 'initial-balances.json', {'wallet':wallet, 'tokens':balances})
        native.save(evidence / 'manifest.json', manifest)
        run_scenario(args, shared, helper, node, evidence, run_id, wallet, key, manifest['oracles'])


def run_scenario(args, shared, helper, node, evidence, run_id, wallet, key, oracles):
    endpoint = node.url
    oracle_lock = threading.Lock()
    class Handler(BaseHTTPRequestHandler):
        def do_POST(self):
            body=self.rfile.read(int(self.headers['Content-Length']))
            request=json.loads(body)
            if request.get('method') == 'simulateTransaction':
                # Fixture keeper refreshes only timestamp fields of cloned aggregate
                # prices; no production oracle/account is modified.
                with oracle_lock:
                    for address in oracles:
                        value = node.rpc('getAccountInfo', [address, {'encoding':'base64'}])['value']
                        if value is None: raise RuntimeError('Fixture oracle is missing')
                        raw = bytearray(base64.b64decode(value['data'][0]))
                        if len(raw) < 185: raise RuntimeError('Fixture oracle layout is invalid')
                        raw[177:185] = int(time.time()).to_bytes(8, 'little')
                        node.rpc('surfnet_setAccount', [address, {'data':raw.hex()}])
                        updated = node.rpc('getAccountInfo', [address, {'encoding':'base64'}])['value']
                        if updated['owner'] != value['owner'] or base64.b64decode(updated['data'][0]) != raw:
                            raise RuntimeError('Native Surfpool oracle timestamp update did not match')
            upstream=urllib.request.Request(endpoint,data=body,headers={'Content-Type':'application/json'})
            with urllib.request.urlopen(upstream,timeout=90) as response: payload=response.read()
            if request.get('method')=='simulateTransaction':
                (evidence/'simulation.json').write_bytes(payload)
            self.send_response(200);self.send_header('Content-Type','application/json');self.send_header('Content-Length',str(len(payload)));self.end_headers();self.wfile.write(payload)
        def log_message(self,*args): pass
    proxy=ThreadingHTTPServer(('127.0.0.1',0),Handler)
    threading.Thread(target=proxy.serve_forever,daemon=True).start()
    name = f'jlp-{run_id}'
    env = dict(os.environ, GOFIN_DEV_STATE_HOME=str(evidence/'state'), GOFIN_SURFPOOL_ADVANCE='1', GOFIN_METEORA_VALIDATOR_RPC=f"http://127.0.0.1:{proxy.server_port}", GOFIN_METEORA_VALIDATOR_WALLET_KEY=str(key), GOFIN_JUPITER_VALIDATOR='jlp')
    shared.ACTIVE_EVIDENCE = {'run_id': run_id}
    started = False
    try:
        shared.start_environment(env,name); started = True
        common = {'network':'solana','cluster':'mainnet-beta','wallet':shared.WALLET_ALIAS,'maximum_network_fee_lamports':'20000','maximum_account_funding_lamports':'0'}
        scope={'wallet':shared.WALLET_ALIAS,'network':'solana','cluster':'mainnet-beta'}
        before=shared.cli(env,name,'read','--input',json.dumps(scope),'wallet.balances','--json')
        def balance(snapshot,mint): return sum(int(a['amount_atoms']) for a in snapshot.get('token_accounts',[]) if a.get('asset',{}).get('reference') == mint)
        initial_usdc=balance(before,shared.USDC)
        if balance(before,JLP) != 0: raise AssertionError('JLP fixture must start empty')
        mint_input=dict(common,deposit_token='USDC',amount_atoms='1000000',minimum_jlp_atoms='1')
        preview=shared.cli(env,name,'preview','jupiter.jlp.mint','--input',json.dumps(mint_input),'--json')
        if sys.platform == 'darwin':
            preview_file=evidence/'preview.json'; preview_file.write_text(json.dumps(preview))
            render_env=dict(env,GOFIN_JUPITER_PREVIEW_FILE=str(preview_file))
            render=subprocess.run(['cargo','test','--locked','-p','gofin-broker','--test','jupiter_live_approval','actual_jupiter_preview_renders_in_native_helper','--','--ignored','--exact'],cwd=ROOT,env=render_env,capture_output=True,text=True,timeout=240)
            if render.returncode or '1 passed; 0 failed; 0 ignored' not in render.stdout:
                raise AssertionError(f'Actual JLP preview did not render in Swift: {render.stdout} {render.stderr}')
        mint = shared.execute(env,name,'jupiter.jlp.mint',mint_input,'jlp-mint')
        credits = [e['received'] for e in shared.effect_rows(mint) if e.get('kind') == 'conversion' and e.get('received',{}).get('asset',{}).get('reference') == JLP and int(e['received']['atoms']) > 0]
        if len(credits)!=1: raise AssertionError('Mint must prove exactly one positive JLP credit')
        amount=credits[0]['atoms']
        observed=shared.cli(env,name,'read','--input',json.dumps({'wallet':shared.WALLET_ALIAS,'network':'solana','cluster':'mainnet-beta'}),'wallet.balances','--json')
        if sum(int(a['amount_atoms']) for a in observed.get('token_accounts',[]) if a.get('asset',{}).get('reference') == JLP) != int(amount): raise AssertionError('Observed JLP balance does not match finalized mint')
        burn=shared.execute(env,name,'jupiter.jlp.burn',dict(common,receive_token='USDC',jlp_atoms=amount,minimum_receive_atoms='1'),'jlp-burn')
        if not any(e.get('kind') == 'conversion' and e.get('received',{}).get('asset',{}).get('reference') == shared.USDC and int(e['received']['atoms']) > 0 for e in shared.effect_rows(burn)): raise AssertionError('Burn must prove positive USDC credit')
        final = shared.cli(env,name,'read','--input',json.dumps({'wallet':shared.WALLET_ALIAS,'network':'solana','cluster':'mainnet-beta'}),'wallet.balances','--json')
        if sum(int(a['amount_atoms']) for a in final.get('token_accounts',[]) if a.get('asset',{}).get('reference') == JLP) != 0:
            raise AssertionError('Burn did not redeem the observed entire JLP balance')
        burned = [e['received'] for e in shared.effect_rows(burn) if e.get('kind') == 'conversion' and e.get('received',{}).get('asset',{}).get('reference') == shared.USDC]
        if len(burned)!=1 or balance(final,shared.USDC) != initial_usdc - 1000000 + int(burned[0]['atoms']):
            raise AssertionError('Observed USDC balance differs from finalized mint and burn conversions')
        summary = {'scenario':'jupiter-jlp','status':'passed','native_review_rendered':sys.platform == 'darwin','production_writes':0,'fresh_environment':str(evidence),'fixture_loading':'surfpool_native','wallet':wallet,'missing_native_capabilities':['Jupiter oracle timestamp keeper; local timestamp fields refreshed via surfnet_setAccount'],'evidence':'owned patched Surfpool, cloned Jupiter program and locally refreshed oracle timestamps', 'mint':{'operation_id':mint.get('id'), 'effects':shared.effect_rows(mint)}, 'burn':{'operation_id':burn.get('id'), 'effects':shared.effect_rows(burn)}, 'observed_jlp_atoms':amount, 'final_jlp_atoms':'0', 'initial_usdc_atoms':str(initial_usdc), 'final_usdc_atoms':str(balance(final,shared.USDC))}
        (evidence/'summary.json').write_text(json.dumps(summary,indent=2)+'\n')
        if args.summary:
            args.summary.parent.mkdir(parents=True,exist_ok=True)
            args.summary.write_text(json.dumps(summary,indent=2)+'\n')
        print(json.dumps(summary))
    finally:
        if started: shared.stop_environment(env,name)
        proxy.shutdown(); proxy.server_close()

if __name__ == '__main__': main()
