"""Synthetic only: owned Anvil receipt model conservation and identity checks."""
import unittest
from types import SimpleNamespace
from aave_fork_receipt_proxy import ReceiptProxy

OWNER='0x'+'11'*20
WETH='0x'+'22'*20
HASH='0x'+'33'*32
META={'instanceId':'fixture-node','forkedNetwork':{'forkBlockHash':'fixture-anchor'}}
class ProxyTests(unittest.TestCase):
    def proxy(self,chain,receipt):
        r=SimpleNamespace(identity=META,owner=OWNER)
        p=ReceiptProxy(r,'http://127.0.0.1:1',chain,WETH)
        def raw(method,params):
            if method=='anvil_metadata':return META
            if method=='eth_getTransactionReceipt':return receipt.copy()
            if method=='eth_getBlockByNumber':return {'transactions':[{'hash':HASH,'from':OWNER,'value':'0xa'}]}
            if method=='eth_getBalance':return hex(1000 if params[1]=='0x9' else 850)
            raise AssertionError(method)
        p.raw=raw
        return p
    def test_base_attributes_only_remaining_local_native_fee(self):
        receipt={'blockNumber':'0xa','transactionHash':HASH,'gasUsed':'0x14','effectiveGasPrice':'0x5','logs':[]}
        value=self.proxy(8453,receipt).request('eth_getTransactionReceipt',[HASH])
        self.assertEqual(value['l1Fee'],'0x28') # 150 debit -10 value -100 execution
        self.assertEqual(value['operatorFeeScalar'],'0x0')
    def test_negative_unexplained_fee_fails(self):
        receipt={'blockNumber':'0xa','transactionHash':HASH,'gasUsed':'0xff','effectiveGasPrice':'0x5','logs':[]}
        with self.assertRaises(AssertionError):self.proxy(8453,receipt).request('eth_getTransactionReceipt',[HASH])
    def test_wrong_fork_identity_fails_before_transport(self):
        p=self.proxy(42161,{})
        p.raw=lambda m,a: {**META,'instanceId':'wrong-node'}
        with self.assertRaises(AssertionError):p.request('eth_sendRawTransaction',['synthetic'])
    def test_nitro_fixture_explicitly_models_zero_l1_gas(self):
        value=self.proxy(42161,{'transactionHash':HASH}).request('eth_getTransactionReceipt',[HASH])
        self.assertEqual(value['gasUsedForL1'],'0x0')
if __name__=='__main__':unittest.main()
