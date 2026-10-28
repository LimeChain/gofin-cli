#!/usr/bin/env python3
"""Public Meta swap round trip against real SPL transfers on Surfpool."""
import base64
import importlib.util
import json
import os
from pathlib import Path
import shutil
import sys
import uuid

ROOT = Path(__file__).resolve().parents[2]
def load(path, name):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module
h = load(ROOT / 'scripts/test-meteora-cli-validator.py', 'meteora_cli_harness')
helper = h.load_validator_helpers()
for tool in ('surfpool', 'solana', 'solana-keygen', 'cargo', 'make'):
    if not shutil.which(tool): raise SystemExit(f'Missing required executable: {tool}')
h.validate_surfpool()
ids = json.loads((h.EVIDENCE / 'synthetic-identities.json').read_text())
wallet, maker = ids['wallet']['pubkey'], 'Gymcte79ALGtFrWybRxqb7M7uRjnbjgaHjGgQf1v4qB8'
USDT = 'Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB'
TOKEN = 'TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA'
run_id = uuid.uuid4().hex[:12]
state = h.CLI_EVIDENCE / f'jupiter-swap-{run_id}'
h.private_directory(state)
rent = 2_039_280
extra = []
accounts = {}
for mint, amount in ((USDT, 100_000_000_000), (h.USDC, 100_000_000_000)):
    address = helper.derive_address(helper.ATA, f'pubkey:{maker}', f'pubkey:{TOKEN}', f'pubkey:{mint}')
    native = False
    raw = helper.synthetic_token_account(mint, maker, amount, native, rent)
    path = state / f'{mint}.json'
    helper.write_genesis_account(path, address, {'lamports': rent + (amount if native else 0), 'data': [base64.b64encode(raw).decode(), 'base64'], 'owner': TOKEN, 'executable': False, 'rentEpoch': 18446744073709551615, 'space': len(raw)})
    extra.append(('maker-token', address, path)); accounts[mint] = address
maker_path = state / 'maker.json'
helper.write_genesis_account(maker_path, maker, {'lamports': 1_000_000_000, 'data': ['', 'base64'], 'owner': '11111111111111111111111111111111', 'executable': False, 'rentEpoch': 18446744073709551615, 'space': 0})
extra.append(('maker', maker, maker_path))
wallet_usdt = helper.derive_address(helper.ATA, f'pubkey:{wallet}', f'pubkey:{TOKEN}', f'pubkey:{USDT}')
wallet_path = state / 'wallet-usdt.json'
wallet_raw = helper.synthetic_token_account(USDT, wallet, 100_000_000, False, rent)
helper.write_genesis_account(wallet_path, wallet_usdt, {'lamports':rent,'data':[base64.b64encode(wallet_raw).decode(),'base64'],'owner':TOKEN,'executable':False,'rentEpoch':18446744073709551615,'space':len(wallet_raw)})
extra.append(('wallet-usdt', wallet_usdt, wallet_path))
port = h.choose_port()
process, ledger, log, rpc_url = h.start_validator(helper, port, 'jupiter-swap', extra)
name = f'jup-swap-{port}-{run_id}'
env = dict(os.environ, GOFIN_DEV_STATE_HOME=str(state / 'broker'), GOFIN_METEORA_VALIDATOR_RPC=rpc_url,
           GOFIN_METEORA_VALIDATOR_WALLET_KEY=str((ROOT / ids['wallet']['path']).resolve()), GOFIN_SURFPOOL_ADVANCE='1', GOFIN_JUPITER_VALIDATOR='swap')
h.ACTIVE_EVIDENCE = {'run_id': run_id}
started = False
rows = {'rpc': rpc_url, 'live_write': False, 'validator_log': str(log.relative_to(ROOT)), 'operations': {}}
def balances():
    return {label: int(helper.rpc(rpc_url, 'getTokenAccountBalance', [address, {'commitment':'finalized'}])['value']['amount']) for label, address in {
        'wallet_usdt': wallet_usdt, 'wallet_usdc': ids['usdc_ata']['pubkey'], 'maker_usdt': accounts[USDT], 'maker_usdc': accounts[h.USDC]}.items()}
try:
    rows['start'] = h.start_environment(env, name); started = True
    before = balances(); rows['before'] = before
    for label, source, destination, amount, minimum in [('forward',USDT,h.USDC,1000000,1000000), ('reverse',h.USDC,USDT,1000000,1000000)]:
        request = {'wallet':h.WALLET_ALIAS, 'network':'solana', 'cluster':'mainnet-beta', 'input_mint':source, 'output_mint':destination, 'amount_atoms':str(amount), 'minimum_output_atoms':str(minimum), 'maximum_slippage_bps':50, 'maximum_platform_fee_bps':0, 'maximum_network_fee_lamports':20000, 'maximum_account_funding_lamports':0}
        rows['operations'][label] = h.execute(env, name, 'jupiter.swap', request, label)
        rows[label + '_balances'] = balances()
    if rows['forward_balances'] != {'wallet_usdt':before['wallet_usdt']-1000000,'wallet_usdc':before['wallet_usdc']+1000000,'maker_usdt':before['maker_usdt']+1000000,'maker_usdc':before['maker_usdc']-1000000}:
        raise AssertionError('Forward finalized SPL balances differ from the ordered amounts')
    if rows['reverse_balances'] != before: raise AssertionError('Round trip did not restore all four SPL balances')
    rows['result'] = 'passed'
finally:
    if started: h.stop_environment(env, name)
    process.terminate(); process.wait(timeout=20)
    (state / 'evidence.json').write_text(json.dumps(rows, indent=2) + '\n')
print(json.dumps({'result':'passed','evidence':str((state/'evidence.json').relative_to(ROOT))}))
