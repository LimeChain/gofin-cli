// Exact native-USDC source and venue evidence for the deposit workflow fixture.
use super::*;
pub(super) const TOKEN: &str = "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48";
pub(super) fn venue_response(venue: &Venue, endpoint: E, credited: bool, out: &mut Value) {
    *out = match endpoint {
        E::KrakenFundingGetFundingV1MethodsDirection => json!({"methods":[{
            "method_id":"00000000-0000-0000-0000-000000000001",
            "method_name":"USDC - Ethereum","asset":{"class":"currency","name":"USDC"},
            "network":{"network_id":"00000000-0000-0000-0000-000000000002","network_name":"Ethereum","contract_address":TOKEN},
            "minimum_amount":"0.01","maximum_amount":"100000",
            "fees":{"base":{"asset":{"class":"currency","name":"USDC"},"amount":"0"},"included":true},
            "deposit":{"shares_addresses_with_method_id":"00000000-0000-0000-0000-000000000003"}
        }]}),
        E::KrakenPublicGetAssets => json!({"result":{"USDC":{"altname":"USDC","decimals":6}}}),
        E::KrakenFundingGetFundingV2DepositAddresses => json!({"addresses":[{
            "method_id":"00000000-0000-0000-0000-000000000001",
            "shares_addresses_with_method_id":"00000000-0000-0000-0000-000000000003",
            "address_details":{"crypto":{"address":"0x1111111111111111111111111111111111111111"}}
        }]}),
        E::KrakenFundingGetFundingV1FeesMethodId => json!({
            "fee":{"asset":{"class":"currency","name":"USDC"},"amount":"0"},
            "gross_amount":{"asset":{"class":"currency","name":"USDC"},"amount":"100"},
            "net_amount":{"asset":{"class":"currency","name":"USDC"},"amount":"100"}
        }),
        E::KrakenFundingGetFundingV1LimitsDepositAssetClassAsset => json!({"deposit_limits":[{
            "method_id":"00000000-0000-0000-0000-000000000001",
            "maximum_amount":{"asset":{"class":"currency","name":"USDC"},"amount":"100000"}
        }]}),
        E::KrakenPrivatePostBalanceEx => {
            json!({"result":{"USDC":{"balance":if credited{"110"}else{"10"},"hold_trade":"0"}}})
        }
        E::KrakenPrivatePostDepositStatus => json!({"result":[{
            "asset":"USDC","method":"USDC - Ethereum (Unified)","refid":"fixture-deposit-ref",
            "txid":venue.rpc.hash(),"info":"0x1111111111111111111111111111111111111111",
            "amount":"100","fee":"0","status":if credited{"Success"}else{"Settled"}
        }]}),
        E::KrakenPrivatePostLedgers => json!({"result":{"ledger":{"deposit-ledger":{
            "type":"deposit","refid":"fixture-deposit-ref","asset":"USDC",
            "amount":"100","fee":"0","balance":"110"
        }}}}),
        _ => return,
    };
}
pub(super) fn rpc_response(rpc: &Rpc, method: &str, parameters: &Value) -> Option<Value> {
    use gofin_evm::alloy::sol_types::SolValue;
    match method {
        "eth_getCode" if parameters[0] == TOKEN => Some(json!("0x6000")),
        "eth_call" if parameters[0]["to"] == TOKEN => {
            let data = parameters[0]["data"]
                .as_str()
                .or_else(|| parameters[0]["input"].as_str())
                .unwrap_or("");
            Some(if data.starts_with("0x313ce567") {
                json!(format!("0x{:064x}", 6))
            } else if data.starts_with("0x95d89b41") || data.starts_with("0x06fdde03") {
                json!(format!("0x{}", hex::encode("USDC".to_owned().abi_encode())))
            } else if data.starts_with("0x70a08231") {
                json!(format!(
                    "0x{:064x}",
                    if rpc.submitted_hash.lock().unwrap().is_some() {
                        400_000_000u64
                    } else {
                        500_000_000u64
                    }
                ))
            } else {
                json!(format!("0x{:064x}", 1))
            })
        }
        "eth_getTransactionReceipt" if rpc.receipt.load(Ordering::SeqCst) => {
            let transaction = rpc.transaction.lock().unwrap();
            let sender = &transaction.as_ref().unwrap().sender;
            Some(json!({
                "transactionHash":rpc.hash(),"blockHash":format!("0x{}","cd".repeat(32)),
                "blockNumber":"0x64","status":"0x1","gasUsed":"0x5208","effectiveGasPrice":"0x3b9aca00",
                "logs":[{
                    "address":TOKEN,
                    "topics":["0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef",
                        format!("0x{}{}","0".repeat(24),sender.trim_start_matches("0x")),
                        format!("0x{}{}","0".repeat(24),"11".repeat(20))],
                    "data":format!("0x{:064x}",100_000_000u64),
                    "transactionHash":rpc.hash(),"blockHash":format!("0x{}","cd".repeat(32)),
                    "blockNumber":"0x64","logIndex":"0x0","removed":false
                }]
            }))
        }
        _ => None,
    }
}
