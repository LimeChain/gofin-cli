#!/usr/bin/env python3
"""Positive public CLI AMM swap and receipt-funded return on an owned fork."""
import argparse
import importlib.util
import json
from pathlib import Path
import tempfile
import time
import uuid

SOURCE = Path(__file__).resolve().parent.parent
SPEC = importlib.util.spec_from_file_location('amm_cli', Path(__file__).with_name('verify-amm-cli-plan-fork.py'))
AMM = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(AMM)

class Roundtrip(AMM.Rehearsal):
    seed_balance = False
    mine_wall_time = True
    fixture_prices = True
    def make_swap(self, dex, protocol, token_in, token_out, amount, fee):
        route = {**AMM.BASE, 'token_in':token_in, 'token_out':token_out, 'amount':amount}
        if dex == 'ramses':
            route['tick_spacing'] = 1
        else:
            route['protocol'] = protocol
            if protocol == 'v3': route['fee_tier'] = fee
            else: route['v4_pool_key'] = {'currency0':'0x'+'0'*40,'currency1':AMM.TOKEN,'fee':500,'tick_spacing':10,'hooks':'0x'+'0'*40}
        quote=self.read(f'{dex}.quote',route)['data']
        assert int(quote['output']) > 0 and quote['price_impact_bps'] <= 100
        request={**route,'request_id':str(uuid.uuid4()),'recipient':self.owner,'deadline':int(time.time())+120,
                 'max_fee_eth':'0.01','slippage_bps':50,'max_price_impact_bps':100,
                 'min_output':AMM.decimal((int(quote['output'])*9950+9999)//10000,quote['token_out']['decimals'])}
        return request

    def run_positive(self,dex,protocol,fee):
        before=self.read('wallet.balances',{**AMM.BASE,'tokens':[AMM.TOKEN]})
        first=self.settled(f'{dex}.swap',self.make_swap(dex,protocol,'native',AMM.TOKEN,'0.001',fee))
        proceeds=AMM.conversion(first)['received']
        assert proceeds['asset']['reference'].lower()==AMM.TOKEN.lower() and int(proceeds['atoms'])>0
        records=[first]
        if protocol=='v4':
            approve={**AMM.BASE,'request_id':str(uuid.uuid4()),'token':AMM.TOKEN,'spender':'0x000000000022d473030f116ddee9f6b43ac78ba3','amount':AMM.decimal(proceeds['atoms'],proceeds['asset']['decimals']),'max_fee_eth':'0.01'}
            records.append(self.settled('wallet.approve',approve))
        second=self.settled(f'{dex}.swap',self.make_swap(dex,protocol,AMM.TOKEN,'native',AMM.decimal(proceeds['atoms'],proceeds['asset']['decimals']),fee))
        records.append(second)
        assert AMM.conversion(second)['spent']==proceeds
        if protocol=='v4':
            revoke={**approve,'request_id':str(uuid.uuid4()),'amount':'0'}
            records.append(self.settled('wallet.approve',revoke))
        after=self.read('wallet.balances',{**AMM.BASE,'tokens':[AMM.TOKEN]})
        def atoms(value, asset):
            return int(next(r['amount']['atoms'] for r in value['data'] if r['amount']['asset']['reference'].lower()==asset.lower()))
        assert atoms(after,AMM.TOKEN)==atoms(before,AMM.TOKEN)
        costs=sum(int(e['amount']['atoms']) for record in records for e in record['effects']['effects'] if e['kind']=='cost' and e['amount']['asset']['reference']=='native')
        spent=int(AMM.conversion(first)['spent']['atoms']);returned=int(AMM.conversion(second)['received']['atoms'])
        assert atoms(after,'native')==atoms(before,'native')-spent+returned-costs
        self.save('result',{'status':'passed','dex':dex,'protocol':protocol,'chain_id':AMM.CHAIN,'forward_id':first['id'],'reverse_id':second['id'],'asset_restored':True,'production_writes':0,'liquidity':'deployed fork state, not synthetic seeded pool','approval':'isolated development automatic','valuation':'synthetic fixed owned-fork prices; not public pricing'})

def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--dex',required=True,choices=('uniswap','sushi','pancakeswap','ramses'))
    parser.add_argument('--protocol',choices=('v3','v4'),default='v3')
    parser.add_argument('--chain',type=int,default=42161)
    parser.add_argument('--fee',type=int,default=500)
    args=parser.parse_args()
    if args.protocol=='v4' and args.dex!='uniswap': parser.error('v4 requires Uniswap')
    if args.chain not in AMM.FORK.reviewed_chains(args.dex): parser.error('unreviewed deployment')
    AMM.CHAIN=args.chain;AMM.TOKEN=AMM.FORK.TOKENS[args.chain];AMM.BASE={'wallet':'dev-wallet','network':'evm','chain_id':args.chain}
    root=SOURCE/'.context'/'test-runs';root.mkdir(parents=True,exist_ok=True)
    evidence=Path(tempfile.mkdtemp(prefix='amm-cli-roundtrip.',dir=root));print(json.dumps({'evidence':str(evidence),'production_writes':0}),flush=True)
    rehearsal=Roundtrip(evidence,f'{args.dex}-{args.protocol}-{args.chain}',args.dex,args.fee)
    try:
        rehearsal.start();rehearsal.run_positive(args.dex,args.protocol,args.fee)
        print(json.dumps({'result':'passed','evidence':str(evidence)}),flush=True)
    finally: rehearsal.close()

if __name__=='__main__': main()
