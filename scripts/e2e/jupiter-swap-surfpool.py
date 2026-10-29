#!/usr/bin/env python3
"""Public Meta swap round trip against native Surfpool SPL accounts."""
import importlib.util
import json
import os
from pathlib import Path
import shutil
import tempfile
import uuid

ROOT = Path(__file__).resolve().parents[2]
def load(path, name):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module
h = load(ROOT / 'scripts/test-meteora-cli-validator.py', 'meteora_cli_harness')
native = h.load_native_surfpool()
for tool in ('surfpool', 'solana', 'solana-keygen', 'cargo', 'make'):
    if not shutil.which(tool): raise SystemExit(f'Missing required executable: {tool}')
h.validate_surfpool()
maker = 'Gymcte79ALGtFrWybRxqb7M7uRjnbjgaHjGgQf1v4qB8'
USDT = 'Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB'
TOKEN = native.TOKEN
run_id = uuid.uuid4().hex[:12]
h.private_directory(h.EVIDENCE)
state = Path(tempfile.mkdtemp(prefix='native-jupiter-swaps-', dir=h.EVIDENCE))
# The synthetic order/execute backend settles SPL transfers. It needs only the
# two deployed token mints and token programs, never Meteora pools or fixtures.
programs = [TOKEN, native.ATA]
addresses = [*programs, USDT, h.USDC]
snapshot = state / 'dependencies.json'
native.save(snapshot, dict.fromkeys(addresses))
rows = {'live_write': False, 'operations': {}, 'fixture_loading':'surfpool_native',
        'fresh_environment':str(state.relative_to(ROOT))}
with native.Surfpool(state / 'execution', snapshot) as node:
    native.validate_preload(node.rpc, addresses, programs, state / 'preload-evidence.json')
    wallet, wallet_key = native.wallet(node.rpc, state, 'wallet', 1_000_000_000_000)
    native.set_native_balance(node.rpc, maker, 1_000_000_000)
    accounts = {}
    balances = []
    for owner, amount in ((maker, 100_000_000_000), (wallet, 100_000_000)):
        for mint in (USDT, h.USDC):
            row = native.fund_token(node.rpc, owner, {'mint':mint,'token_program':TOKEN,'decimals':6}, amount)
            balances.append(row)
            accounts[(owner,mint)] = row['address']
    native.save(state / 'initial-balances.json', balances)
    name = f'jup-swap-{run_id}'
    env = dict(os.environ, GOFIN_DEV_STATE_HOME=str(state / 'broker'), GOFIN_METEORA_VALIDATOR_RPC=node.url,
               GOFIN_METEORA_VALIDATOR_WALLET_KEY=str(wallet_key), GOFIN_SURFPOOL_ADVANCE='1', GOFIN_JUPITER_VALIDATOR='swap')
    h.ACTIVE_EVIDENCE = {'run_id': run_id}
    started = False
    rows.update(rpc=node.url, wallet=wallet, validator_log=str((node.directory/'surfpool.log').relative_to(ROOT)))
    def read_balances():
        return {label: int(node.rpc('getTokenAccountBalance', [accounts[(owner,mint)], {'commitment':'finalized'}])['value']['amount'])
                for label, owner, mint in [('wallet_usdt',wallet,USDT), ('wallet_usdc',wallet,h.USDC),
                                          ('maker_usdt',maker,USDT), ('maker_usdc',maker,h.USDC)]}
    try:
        rows['start'] = h.start_environment(env, name); started = True
        before = read_balances(); rows['before'] = before
        for label, source, destination, amount, minimum in [('forward',USDT,h.USDC,1000000,1000000), ('reverse',h.USDC,USDT,1000000,1000000)]:
            request = {'wallet':h.WALLET_ALIAS, 'network':'solana', 'cluster':'mainnet-beta', 'input_mint':source, 'output_mint':destination, 'amount_atoms':str(amount), 'minimum_output_atoms':str(minimum), 'maximum_slippage_bps':50, 'maximum_platform_fee_bps':0, 'maximum_network_fee_lamports':20000, 'maximum_account_funding_lamports':0}
            rows['operations'][label] = h.execute(env, name, 'jupiter.swap', request, label)
            rows[label + '_balances'] = read_balances()
        if rows['forward_balances'] != {'wallet_usdt':before['wallet_usdt']-1000000,'wallet_usdc':before['wallet_usdc']+1000000,'maker_usdt':before['maker_usdt']+1000000,'maker_usdc':before['maker_usdc']-1000000}:
            raise AssertionError('Forward finalized SPL balances differ from the ordered amounts')
        if rows['reverse_balances'] != before: raise AssertionError('Round trip did not restore all four SPL balances')
        rows['result'] = 'passed'
    finally:
        if started: h.stop_environment(env, name)
        native.save(state / 'evidence.json', rows)
print(json.dumps({'result':'passed','evidence':str((state/'evidence.json').relative_to(ROOT))}))
