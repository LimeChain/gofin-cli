//! Owned Anvil-only aggregate OP fee model; never used by a live adapter.
//! Anvil omits authentic receipt fee components. Trace-observed native inflows
//! and one-transaction balance conservation recover its total extra debit.
use super::*;
use alloy::primitives::{U256, keccak256};

fn invalid(message: &str) -> FinError {
    FinError::validation(message)
}
fn native_inflow(frame: &Value, owner: &str) -> Result<U256> {
    if frame.get("error").is_some() {
        return Ok(U256::ZERO);
    }
    let mut total = U256::ZERO;
    if frame["type"] == "CALL"
        && frame["to"]
            .as_str()
            .is_some_and(|to| to.eq_ignore_ascii_case(owner))
    {
        total = frame
            .get("value")
            .map(rpc_u256)
            .transpose()?
            .unwrap_or(U256::ZERO);
    }
    if let Some(calls) = frame.get("calls").and_then(Value::as_array) {
        for child in calls {
            total = total
                .checked_add(native_inflow(child, owner)?)
                .ok_or_else(|| invalid("Fork native inflow overflowed"))?;
        }
    }
    Ok(total)
}

impl OwnedRpc {
    pub(super) async fn base_fee_call(&self, params: &Value) -> Result<Option<Value>> {
        if self.chain != 8453
            || !params[0]["to"].as_str().is_some_and(|to| {
                to.eq_ignore_ascii_case("0x420000000000000000000000000000000000000f")
                    || to.eq_ignore_ascii_case("0x4200000000000000000000000000000000000015")
            })
        {
            return Ok(None);
        }
        let data = params[0]["data"].as_str().unwrap_or_default();
        let selector = |sig: &str| format!("0x{}", hex::encode(&keccak256(sig)[..4]));
        let value = if data.starts_with(&selector("isFjord()")) {
            U256::from(1)
        } else if data.starts_with(&selector("getL1FeeUpperBound(uint256)")) {
            let l1 = rpc_u256(&self.raw("eth_call", params.clone()).await?)?;
            // All fork writes are bounded below two million gas. Aggregate the
            // real pinned operator upper bound without weakening the reserve.
            let data = format!(
                "{}{:064x}",
                selector("getOperatorFee(uint256)"),
                U256::from(2_000_000)
            );
            let operator = rpc_u256(&self.raw("eth_call", json!([
                {"to":"0x420000000000000000000000000000000000000f", "data":data}, params[1]
            ])).await?)?;
            l1.checked_add(operator)
                .ok_or_else(|| invalid("Fork aggregate fee bound overflowed"))?
        } else {
            U256::ZERO
        };
        Ok(Some(json!(format!("0x{value:064x}"))))
    }

    pub(super) async fn base_fee_receipt(&self, result: &mut Value) -> Result<()> {
        if self.chain != 8453 || !result.is_object() {
            return Ok(());
        }
        let block = rpc_u64(&result["blockNumber"])?;
        let header = self
            .raw("eth_getBlockByNumber", json!([result["blockNumber"], true]))
            .await?;
        let txs = header["transactions"]
            .as_array()
            .ok_or_else(|| invalid("Owned fork block transactions missing"))?;
        if txs.len() != 1 || txs[0]["hash"] != result["transactionHash"] {
            return Err(invalid(
                "Only one exact transaction permits fork fee aggregation",
            ));
        }
        let tx = &txs[0];
        let owner =
            lower_address(LocalEnvelopeSigner::from_private_key(PUBLIC_ANVIL_KEY)?.address());
        if tx["from"] != owner || rpc_u64(&tx["gas"])? > 2_000_000 {
            return Err(invalid("Unexpected fork fee payer or gas ceiling"));
        }
        let trace = self
            .raw(
                "debug_traceTransaction",
                json!([
                    result["transactionHash"], {"tracer":"callTracer"}
                ]),
            )
            .await?;
        if trace["from"] != tx["from"] || trace["to"] != tx["to"] || trace.get("error").is_some() {
            return Err(invalid(
                "Owned native inflow trace does not match successful transaction",
            ));
        }
        let inflow = native_inflow(&trace, &owner)?;
        let before = rpc_u256(
            &self
                .raw(
                    "eth_getBalance",
                    json!([owner, format!("{:#x}", block - 1)]),
                )
                .await?,
        )?;
        let after = rpc_u256(
            &self
                .raw("eth_getBalance", json!([owner, result["blockNumber"]]))
                .await?,
        )?;
        let value = rpc_u256(&tx["value"])?;
        let execution = rpc_u256(&result["gasUsed"])? * rpc_u256(&result["effectiveGasPrice"])?;
        let total = before
            .checked_add(inflow)
            .and_then(|q| q.checked_sub(after))
            .and_then(|q| q.checked_sub(value))
            .ok_or_else(|| invalid("Fork native conservation failed"))?;
        let extras = total
            .checked_sub(execution)
            .ok_or_else(|| invalid("Fork execution fee exceeds total observed cost"))?;
        result["l1Fee"] = json!(format!("{extras:#x}"));
        result["operatorFeeScalar"] = json!("0x0");
        result["operatorFeeConstant"] = json!("0x0");
        Ok(())
    }
}
