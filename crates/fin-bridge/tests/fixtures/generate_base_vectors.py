"""Regenerate unmistakably synthetic Base bridge vectors with Foundry cast.

ABI/layout source: base/contracts 5f915b1e471457fa924b8daabdc5289eca361f24.
No RPC, account keys, wallet credentials, or actual financial transactions.
"""
import json
import os
import subprocess
from pathlib import Path


def cast(*args):
    return subprocess.check_output(['cast', *map(str, args)], text=True, env={**os.environ, 'FOUNDRY_DISABLE_NIGHTLY_WARNING':'1'}).strip()


def abi(signature, *args):
    return cast('abi-encode', signature, *args)


def keccak(data):
    return cast('keccak', data)


def word_address(address):
    return '0x' + '00' * 12 + address[2:]


def log(index, emitter, signature, topics, data):
    return dict(log_index=index, address=emitter, topics=[keccak(signature), *topics], data=data)


L1_BRIDGE = '0x3154cf16ccdb4c6d922629664174b904d80f2c35'
L2_BRIDGE = '0x4200000000000000000000000000000000000010'
L1_MESSENGER = '0x866e82a600a1414e583f7f13623f1ac5d58b0afa'
L2_MESSENGER = '0x4200000000000000000000000000000000000007'
PORTAL = '0x49048044d57e1c92a77f79988d21fa8faf74e97e'
PASSER = '0x4200000000000000000000000000000000000016'
FROM = '0x1111111111111111111111111111111111111111'
TO = '0x2222222222222222222222222222222222222222'
L1_TOKEN = '0x3333333333333333333333333333333333333333'
L2_TOKEN = '0x4444444444444444444444444444444444444444'
BLOCK = '0x' + '55' * 32
AMOUNT = 1234567
NONCE = (1 << 240) + 42
GAS = 350000
vectors = []
for source, token in [(1, False), (1, True), (8453, False), (8453, True)]:
    dest = 8453 if source == 1 else 1
    source_bridge, dest_bridge = (L1_BRIDGE, L2_BRIDGE) if source == 1 else (L2_BRIDGE, L1_BRIDGE)
    source_token, dest_token = (L1_TOKEN, L2_TOKEN) if source == 1 else (L2_TOKEN, L1_TOKEN)
    intent = dict(wallet='fixture:synthetic-wallet', source_address=FROM,
                  source=dict(chain_id=source, token=source_token if token else None, decimals=6 if token else 18),
                  destination_chain_id=dest, destination_assets=[dict(chain_id=dest, token=dest_token if token else None, decimals=6 if token else 18)],
                  recipient=TO, refund_to=FROM, allowed_refund_assets=[], amount=str(AMOUNT))
    if token:
        inner = cast('calldata', 'finalizeBridgeERC20(address,address,address,address,uint256,bytes)', dest_token, source_token, FROM, TO, AMOUNT, '0x')
        source_log = log(31, source_bridge, 'ERC20BridgeInitiated(address,address,address,address,uint256,bytes)',
                         [word_address(source_token), word_address(dest_token), word_address(FROM)], abi('f(address,uint256,bytes)', TO, AMOUNT, '0x'))
        dest_log = log(8, dest_bridge, 'ERC20BridgeFinalized(address,address,address,address,uint256,bytes)',
                       [word_address(dest_token), word_address(source_token), word_address(FROM)], abi('f(address,uint256,bytes)', TO, AMOUNT, '0x'))
    else:
        inner = cast('calldata', 'finalizeBridgeETH(address,address,uint256,bytes)', FROM, TO, AMOUNT, '0x')
        source_log = log(31, source_bridge, 'ETHBridgeInitiated(address,address,uint256,bytes)', [word_address(FROM),word_address(TO)], abi('f(uint256,bytes)', AMOUNT, '0x'))
        dest_log = log(8, dest_bridge, 'ETHBridgeFinalized(address,address,uint256,bytes)', [word_address(FROM),word_address(TO)], abi('f(uint256,bytes)', AMOUNT, '0x'))
    value = 0 if token else AMOUNT
    relay = cast('calldata','relayMessage(uint256,address,address,uint256,uint256,bytes)',NONCE,source_bridge,dest_bridge,value,200000,inner)
    message_hash = keccak(relay)
    dest_log2 = log(9, L2_MESSENGER if source == 1 else L1_MESSENGER, 'RelayedMessage(bytes32)', [message_hash], '0x')
    case = dict(intent=intent, source_block_hash=BLOCK, message_hash=message_hash, destination_logs=[dest_log, dest_log2])
    if source == 1:
        aliased = '0x' + f'{(int(L1_MESSENGER,16) + int("1111000000000000000000000000000000001111",16)) % (1 << 160):040x}'
        opaque='0x'+f'{value:064x}{value:064x}{GAS:016x}00'+relay[2:]
        source_log2=log(33,PORTAL,'TransactionDeposited(address,address,uint256,bytes)',[word_address(aliased),word_address(L2_MESSENGER),'0x'+'00'*32],abi('f(bytes)',opaque))
        source_hash=keccak(abi('f(uint256,bytes32)',0,keccak(abi('f(bytes32,uint256)',BLOCK,33))))
        encoded = cast('to-rlp', json.dumps([source_hash,aliased,L2_MESSENGER,'0x' if value == 0 else '0x'+f'{value:x}'.rjust((len(f'{value:x}')+1)//2*2,'0'),'0x' if value == 0 else '0x'+f'{value:x}'.rjust((len(f'{value:x}')+1)//2*2,'0'),'0x'+f'{GAS:x}'.rjust((len(f'{GAS:x}')+1)//2*2,'0'),'0x',relay]))
        case.update(source_hash=source_hash,transaction_hash=keccak('0x7e'+encoded[2:]))
    else:
        withdrawal=dict(nonce='77',sender=L2_MESSENGER,target=L1_MESSENGER,value=str(value),gas_limit=str(GAS),data=relay)
        withdrawal_hash=keccak(abi('f(uint256,address,address,uint256,uint256,bytes)',77,L2_MESSENGER,L1_MESSENGER,value,GAS,relay))
        source_log2=log(33,PASSER,'MessagePassed(uint256,address,address,uint256,uint256,bytes,bytes32)',[abi('f(uint256)',77),word_address(L2_MESSENGER),word_address(L1_MESSENGER)],abi('f(uint256,uint256,bytes,bytes32)',value,GAS,relay,withdrawal_hash))
        case.update(withdrawal=withdrawal,withdrawal_hash=withdrawal_hash,storage_key=keccak(abi('f(bytes32,uint256)',withdrawal_hash,0)),finalize_call=cast('calldata','finalizeWithdrawalTransactionExternalProof((uint256,address,address,uint256,uint256,bytes),address)',f'(77,{L2_MESSENGER},{L1_MESSENGER},{value},{GAS},{relay})',FROM))
        version='0x'+'00'*32
        state_root='0x'+'66'*32
        storage_root='0x'+'77'*32
        latest_hash='0x'+'88'*32
        output_root=keccak(abi('f(bytes32,bytes32,bytes32,bytes32)',version,state_root,storage_root,latest_hash))
        witness=dict(dispute_game_index=7,checkpoint_block=100,version=version,state_root=state_root,message_passer_storage_root=storage_root,latest_block_hash=latest_hash,withdrawal_proof=['0xc0','0xdead'],output_root=output_root)
        case.update(witness=witness,prove_call=cast('calldata','proveWithdrawalTransaction((uint256,address,address,uint256,uint256,bytes),uint256,(bytes32,bytes32,bytes32,bytes32),bytes[])',f'(77,{L2_MESSENGER},{L1_MESSENGER},{value},{GAS},{relay})',7,f'({version},{state_root},{storage_root},{latest_hash})','[0xc0,0xdead]'))
    case['source_logs']=[source_log,source_log2]
    if token:
        source_transfer=log(30,source_token,'Transfer(address,address,uint256)',[word_address(FROM),word_address(source_bridge if source == 1 else '0x'+'00'*20)],abi('f(uint256)',AMOUNT))
        destination_transfer=log(7,dest_token,'Transfer(address,address,uint256)',[word_address('0x'+'00'*20 if source == 1 else dest_bridge),word_address(TO)],abi('f(uint256)',AMOUNT))
        case['source_logs'].append(source_transfer)
        case['destination_logs'].append(destination_transfer)
    vectors.append(case)
output=dict(provenance=dict(cast=cast('--version'),contracts_revision='5f915b1e471457fa924b8daabdc5289eca361f24',synthetic=True),vectors=vectors)
Path(__file__).with_name('base-canonical-vectors.json').write_text(json.dumps(output,indent=2)+'\n')
