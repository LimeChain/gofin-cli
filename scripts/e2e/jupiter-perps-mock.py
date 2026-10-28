#!/usr/bin/env python3
"""Positive Jupiter Perps public CLI lifecycle on an isolated mocked backend."""
import argparse
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0,str(ROOT/'scripts'))
spec=importlib.util.spec_from_file_location('perps_rehearsal',ROOT/'scripts/test-solana-rehearsal.py')
h=importlib.util.module_from_spec(spec);spec.loader.exec_module(h)

def main():
    parser=argparse.ArgumentParser();parser.add_argument('--summary',type=Path);args=parser.parse_args()
    with tempfile.TemporaryDirectory(prefix='gofin-perps-positive-') as tmp:
        state=Path(tmp)/'state';name='perps-positive'
        env=dict(os.environ,GOFIN_DEV_STATE_HOME=str(state),GOFIN_DEV_SOLANA_FIXTURE_STATE=str(state/'ledger.json'),GOFIN_DEV_SOLANA_PERPS_REHEARSAL='1')
        started=False
        def call(*argv): return h.call(name,list(argv),env)
        def execute(op,request,identity):
            result=call('execute',op,'--input',json.dumps(request),'--id',identity,'--json')
            if result.get('status')!='completed' or not (result.get('effects') or {}).get('proof_digest'):
                raise AssertionError(f'{identity} did not complete with verified synthetic evidence: {json.dumps(result)}')
            return result
        scope={'network':'solana','cluster':'mainnet-beta','wallet':'solana-rehearsal'}
        try:
            start=subprocess.run([sys.executable,str(h.DEV),'start',name,'--mode','synthetic','--fixture','solana'],cwd=ROOT,env=env,capture_output=True,text=True,timeout=240)
            if start.returncode: raise RuntimeError(start.stderr or start.stdout)
            started=True
            initial=call('read','--input',json.dumps(scope),'wallet.balances','--json')
            positions=call('read','--input',json.dumps(scope),'jupiter.perps.positions','--json')
            if positions.get('positions'): raise AssertionError('Perps fixture must start without positions')
            summary={'scenario':'jupiter-perpetuals','evidence':'mocked Solana RPC and stateful synthetic keeper; public CLI/broker/engine/adapters', 'native_review_rendered':sys.platform == 'darwin', 'production_writes':0,'sides':{}}
            for side in ('long','short'):
                opening=dict(scope,market='SOL',side=side,funding_token='USDC',collateral_amount_atoms='11000000',size_usd_atoms='12100000',maximum_protocol_fee_atoms='5000000',maximum_protocol_fee_usd_atoms='500000',maximum_slippage_bps=50,maximum_network_fee_lamports='10000',maximum_account_funding_lamports='6000000')
                if side == 'long' and sys.platform == 'darwin':
                    preview=call('preview','jupiter.perps.increase_market','--input',json.dumps(opening),'--json')
                    preview_file=Path(tmp)/'preview.json';preview_file.write_text(json.dumps(preview))
                    render_env=dict(env,GOFIN_JUPITER_PREVIEW_FILE=str(preview_file))
                    render=subprocess.run(['cargo','test','--locked','-p','gofin-broker','--test','jupiter_live_approval','actual_jupiter_preview_renders_in_native_helper','--','--ignored','--exact'],cwd=ROOT,env=render_env,capture_output=True,text=True,timeout=240)
                    if render.returncode or '1 passed; 0 failed; 0 ignored' not in render.stdout:
                        raise AssertionError(f'Actual Perps preview did not render in Swift: {render.stdout} {render.stderr}')
                opened=execute('jupiter.perps.increase_market',opening,f'perps-{side}-open')
                read=call('read','--input',json.dumps(scope),'jupiter.perps.positions','--json')
                matches=[p for p in read.get('positions',[]) if p.get('side')==side]
                if len(matches)!=1 or len(read['positions'])!=1: raise AssertionError(f'{side} position not uniquely observed')
                position=matches[0]['position']
                if side == 'long':
                    collateral_before=int(matches[0]['collateral_usd_atoms'])
                    steps=json.loads(h.perps_collateral_plan(initial['wallet'],position))['steps']
                    added=execute(steps[0]['operation'],steps[0]['input'],'perps-collateral-add')
                    added_position=call('read','--input',json.dumps(scope),'jupiter.perps.positions','--json')['positions'][0]
                    if added_position['position'] != position or int(added_position['collateral_usd_atoms']) <= collateral_before or int(added_position['size_usd_atoms']) != 12100000:
                        raise AssertionError('Collateral add did not increase the same position without changing exposure')
                    removed=execute(steps[1]['operation'],steps[1]['input'],'perps-collateral-remove')
                    removed_position=call('read','--input',json.dumps(scope),'jupiter.perps.positions','--json')['positions'][0]
                    if removed_position['position'] != position or int(removed_position['collateral_usd_atoms']) >= int(added_position['collateral_usd_atoms']) or int(removed_position['size_usd_atoms']) != 12100000:
                        raise AssertionError('Collateral remove did not reduce the same position without changing exposure')
                    summary['collateral']={'add':h.summary(added),'remove':h.summary(removed),'before':collateral_before,'after_add':added_position['collateral_usd_atoms'],'after_remove':removed_position['collateral_usd_atoms']}
                closing=dict(scope,position=position,receive_token='USDC',size_usd_atoms='12100000',minimum_receive_atoms='10943010',maximum_protocol_fee_usd_atoms='500000',maximum_slippage_bps=50,maximum_network_fee_lamports='10000',maximum_account_funding_lamports='6000000')
                closed=execute('jupiter.perps.decrease_market',closing,f'perps-{side}-close')
                if call('read','--input',json.dumps(scope),'jupiter.perps.positions','--json').get('positions'): raise AssertionError(f'{side} remained open')
                summary['sides'][side]={'position':position,'open':h.summary(opened),'close':h.summary(closed)}
            order=dict(scope,market='SOL',side='long',funding_token='USDC',collateral_amount_atoms='100000',size_usd_atoms='110000',trigger_price_usd_atoms='150000000',trigger_above_threshold=True,maximum_protocol_fee_atoms='100000',maximum_protocol_fee_usd_atoms='100000',maximum_network_fee_lamports=20000,maximum_account_funding_lamports='7273200')
            placed=call('execute','jupiter.perps.limit.create','--input',json.dumps(order),'--id','perps-limit-open','--json')
            if not h.has_verified_open_limit_progress(placed): raise AssertionError('Resting order did not prove funded open escrow')
            op_id=placed['id'];request=(placed.get('preview') or {}).get('binding',{}).get('request')
            if not isinstance(request,str): raise AssertionError('Missing exact resting request identity')
            cancelled=execute('jupiter.perps.limit.cancel',dict(scope,request=request,original_operation_id=op_id,maximum_network_fee_lamports='5000'),'perps-limit-cancel')
            parent=h.call_pending(name,['status',op_id,'--json'],env)
            if parent.get('status')!='completed' or not (parent.get('effects') or {}).get('proof_digest'): raise AssertionError('Original resting order did not settle from cancellation proof')
            final=call('read','--input',json.dumps(scope),'wallet.balances','--json')
            if h.wallet_token_amount(final,h.USDC_MINT) != h.wallet_token_amount(initial,h.USDC_MINT)-6000: raise AssertionError('Final USDC does not match six exact protocol fees')
            final_positions=call('read','--input',json.dumps(scope),'jupiter.perps.positions','--json')
            if final_positions.get('positions') or any(r.get('request')==request for r in final_positions.get('pending_requests',[])): raise AssertionError('Cancelled request or open position remains')
            summary.update(resting={'request':request,'placement':h.summary(placed),'cancel':h.summary(cancelled),'parent':h.summary(parent)},final_usdc_atoms=str(h.wallet_token_amount(final,h.USDC_MINT)),status='passed')
            if args.summary: args.summary.parent.mkdir(parents=True,exist_ok=True);args.summary.write_text(json.dumps(summary,indent=2)+'\n')
            print(json.dumps(summary))
        finally:
            if started: subprocess.run([sys.executable,str(h.DEV),'stop',name],cwd=ROOT,env=env,capture_output=True,text=True,timeout=60)

if __name__=='__main__':main()
