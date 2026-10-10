//! Base's extra fees are pre-dispatch reservations, not EIP-1559 enforced caps.
//! Evidence uses canonical full blocks, never the pending Flashblock state.
use super::*;
use alloy::sol;

const BASE: u64 = 8453;
const ORACLE: &str = "0x420000000000000000000000000000000000000f";
const ATTRIBUTES: &str = "0x4200000000000000000000000000000000000015";
sol! {
    function isFjord() external view returns (bool);
    function isIsthmus() external view returns (bool);
    function isJovian() external view returns (bool);
    function getL1FeeUpperBound(uint256 size) external view returns (uint256);
    function getOperatorFee(uint256 gas) external view returns (uint256);
    function operatorFeeScalar() external view returns (uint32);
    function operatorFeeConstant() external view returns (uint64);
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OperatorFeeModel {
    PreIsthmus,
    Isthmus,
    Jovian,
}

/// Immutable approval evidence; no signed transaction bytes or signing material.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RollupFeeBound {
    pub version: u8,
    pub observed_block: u64,
    pub observed_block_hash: String,
    pub operator_model: OperatorFeeModel,
    /// Unsigned type-2 byte length, with maximum nonce and fee encodings.
    /// GasPriceOracle adds signature padding internally.
    pub unsigned_size: u64,
    pub maximum_l1_data_fee: Quantity,
    pub maximum_operator_fee: Quantity,
}

fn invalid() -> FinError {
    error(
        "ROLLUP_FEE_EVIDENCE_INVALID",
        "validation",
        "Base requires reviewed canonical fee evidence; obtain a new preview.",
    )
}

fn unsigned_size(transaction: &FrozenTransaction) -> Result<u64> {
    let mut tx = transaction.consensus()?;
    // Size cannot increase when prepare reduces the fee prices after reservation.
    tx.nonce = u64::MAX;
    tx.max_fee_per_gas = u128::MAX;
    tx.max_priority_fee_per_gas = u128::MAX;
    u64::try_from(tx.encoded_for_signing().len()).map_err(|_| invalid())
}

pub(super) fn bound_cost(transaction: &FrozenTransaction) -> Result<U256> {
    match (transaction.chain_id, &transaction.rollup_fee) {
        (BASE, Some(bound)) if bound.version == 1 => {
            if bound.unsigned_size != unsigned_size(transaction)? {
                return Err(invalid());
            }
            hash(&bound.observed_block_hash)?;
            u256(&bound.maximum_l1_data_fee)?
                .checked_add(u256(&bound.maximum_operator_fee)?)
                .ok_or_else(invalid)
        }
        (BASE, _) | (_, Some(_)) => Err(invalid()),
        (_, None) => Ok(U256::ZERO),
    }
}

fn operator_cost(model: OperatorFeeModel, gas: u64, scalar: u32, constant: u64) -> U256 {
    let scaled = U256::from(gas) * U256::from(scalar);
    match model {
        OperatorFeeModel::PreIsthmus => U256::ZERO,
        OperatorFeeModel::Isthmus => scaled / U256::from(1_000_000) + U256::from(constant),
        OperatorFeeModel::Jovian => scaled * U256::from(100) + U256::from(constant),
    }
}

impl EvmRuntime {
    async fn fee_call<C: SolCall>(&self, to: &str, call: &C, block: &Value) -> Result<C::Return> {
        let block_hash = hash(block["hash"].as_str().unwrap_or(""))?;
        // EIP-1898 pins all parameter reads to the same canonical state.
        let raw = rpc_bytes(
            &self
                .rpc
                .request(
                    "eth_call",
                    json!([
                        {"to":to,"data":hex_bytes(&call.abi_encode())},
                        {"blockHash":block_hash.to_string(),"requireCanonical":true}
                    ]),
                )
                .await?,
        )?;
        C::abi_decode_returns(&raw).map_err(|_| invalid())
    }

    async fn fee_model(&self, block: &Value) -> Result<OperatorFeeModel> {
        if !self.fee_call(ORACLE, &isFjordCall {}, block).await? {
            return Err(invalid());
        }
        let isthmus = self.fee_call(ORACLE, &isIsthmusCall {}, block).await?;
        let jovian = self.fee_call(ORACLE, &isJovianCall {}, block).await?;
        let model = match (isthmus, jovian) {
            (false, false) => OperatorFeeModel::PreIsthmus,
            (true, false) => OperatorFeeModel::Isthmus,
            (true, true) => OperatorFeeModel::Jovian,
            (false, true) => return Err(invalid()),
        };
        // Reject unknown header fee semantics rather than assuming a future
        // upgrade still charges exactly the reviewed fork's operator formula.
        let extra = rpc_bytes(&block["extraData"])?;
        let valid_header = match model {
            OperatorFeeModel::Jovian => extra.len() == 17 && extra[0] == 1,
            OperatorFeeModel::Isthmus => extra.len() == 9 && extra[0] == 0,
            OperatorFeeModel::PreIsthmus => extra.len() == 9 && extra[0] == 0,
        };
        if !valid_header {
            return Err(invalid());
        }
        Ok(model)
    }

    /// Reserve all Base fee components, preserving other chains' fee semantics.
    /// This oracle estimate is conservative but does not constrain what the
    /// sequencer can charge after dispatch. Settlement always records real cost.
    async fn rollup_fee_evidence(
        &self,
        transaction: &FrozenTransaction,
    ) -> Result<Option<RollupFeeBound>> {
        if transaction.chain_id != self.chain_id {
            return Err(invalid());
        }
        if self.chain_id != BASE {
            return Ok(None);
        }
        self.assert_chain().await?;
        let block = self
            .rpc
            .request("eth_getBlockByNumber", json!(["latest", false]))
            .await?;
        let number = rpc_u64(&block["number"])?;
        let block_hash = hash(block["hash"].as_str().unwrap_or(""))?;
        let model = self.fee_model(&block).await?;
        let size = unsigned_size(transaction)?;
        let l1 = self
            .fee_call(
                ORACLE,
                &getL1FeeUpperBoundCall {
                    size: U256::from(size),
                },
                &block,
            )
            .await?;
        let operator = self
            .fee_call(
                ORACLE,
                &getOperatorFeeCall {
                    gas: U256::from(transaction.gas),
                },
                &block,
            )
            .await?;
        if model == OperatorFeeModel::PreIsthmus && operator != U256::ZERO {
            return Err(invalid());
        }
        Ok(Some(RollupFeeBound {
            version: 1,
            observed_block: number,
            observed_block_hash: block_hash.to_string(),
            operator_model: model,
            unsigned_size: size,
            maximum_l1_data_fee: quantity(l1),
            maximum_operator_fee: quantity(operator),
        }))
    }

    pub async fn rollup_fee_bound(
        &self,
        transaction: &FrozenTransaction,
    ) -> Result<Option<RollupFeeBound>> {
        let Some(mut bound) = self.rollup_fee_evidence(transaction).await? else {
            return Ok(None);
        };
        bound.maximum_l1_data_fee =
            quantity(ceil_percent(u256(&bound.maximum_l1_data_fee)?, 150).ok_or_else(invalid)?);
        bound.maximum_operator_fee =
            quantity(ceil_percent(u256(&bound.maximum_operator_fee)?, 150).ok_or_else(invalid)?);
        Ok(Some(bound))
    }

    pub(super) async fn revalidate_rollup_fee(
        &self,
        transaction: &FrozenTransaction,
    ) -> Result<()> {
        bound_cost(transaction)?;
        if self.chain_id != BASE {
            return Ok(());
        }
        let previous = transaction.rollup_fee.as_ref().ok_or_else(invalid)?;
        let current = self
            .rollup_fee_evidence(transaction)
            .await?
            .ok_or_else(invalid)?;
        // Fresh estimates must fit the reserved headroom. An upgrade changes fee
        // semantics and requires a fresh preview, even when today it costs less.
        if current.operator_model != previous.operator_model
            || current.maximum_l1_data_fee > previous.maximum_l1_data_fee
            || current.maximum_operator_fee > previous.maximum_operator_fee
        {
            return Err(error(
                "FEE_BOUND_CHANGED",
                "submission",
                "Base data or operator fees exceed the approved reservation; obtain a fresh preview.",
            ));
        }
        Ok(())
    }

    pub(super) async fn receipt_total_fee(
        &self,
        receipt: &Value,
        reported: &Value,
        block: &Value,
        gas: u64,
        price: U256,
    ) -> Result<U256> {
        let execution = U256::from(gas).checked_mul(price).ok_or_else(invalid)?;
        if self.chain_id != BASE {
            return Ok(execution);
        }
        // OP deposit transactions do not buy gas and are exempt from every fee.
        // Do not use ordinary type-2 evidence requirements for derived deposits.
        if rpc_u64(&reported["type"])? == 0x7e {
            if price != U256::ZERO {
                return Err(invalid());
            }
            return Ok(U256::ZERO);
        }
        if rpc_u64(&reported["chainId"])? != BASE {
            return Err(invalid());
        }
        let l1 = rpc_u256(&receipt["l1Fee"])?;
        let model = self.fee_model(block).await?;
        let (scalar, constant) = if model == OperatorFeeModel::PreIsthmus {
            (0, 0)
        } else {
            (
                self.fee_call(ATTRIBUTES, &operatorFeeScalarCall {}, block)
                    .await?,
                self.fee_call(ATTRIBUTES, &operatorFeeConstantCall {}, block)
                    .await?,
            )
        };
        // RPC legitimately omits both fields when both parameters are zero.
        // Their absence alone never proves zero cost.
        if receipt.get("operatorFeeScalar").is_some()
            || receipt.get("operatorFeeConstant").is_some()
        {
            if rpc_u256(&receipt["operatorFeeScalar"])? != U256::from(scalar)
                || rpc_u256(&receipt["operatorFeeConstant"])? != U256::from(constant)
            {
                return Err(invalid());
            }
        } else if scalar != 0 || constant != 0 {
            return Err(invalid());
        }
        let operator = operator_cost(model, gas, scalar, constant);
        execution
            .checked_add(l1)
            .and_then(|v| v.checked_add(operator))
            .ok_or_else(invalid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct Fixture {
        l1: Mutex<U256>,
        scalar: u32,
        constant: u64,
        jovian: bool,
        balance: U256,
        tx: Mutex<Option<FrozenTransaction>>,
        receipt: Mutex<Value>,
    }
    impl Fixture {
        fn new() -> Self {
            Self {
                l1: Mutex::new(U256::from(100)),
                scalar: 1,
                constant: 7,
                jovian: true,
                balance: U256::from(10_u64).pow(U256::from(20)),
                tx: Mutex::new(None),
                receipt: Mutex::new(Value::Null),
            }
        }
        fn block() -> Value {
            json!({"number":"0x10","hash":format!("0x{}", "22".repeat(32)), "extraData":format!("0x01{}", "00".repeat(16))})
        }
    }
    #[async_trait]
    impl EvmRpc for Fixture {
        async fn request(&self, method: &str, params: Value) -> Result<Value> {
            match method {
                "eth_chainId" => Ok(json!("0x2105")),
                "eth_getBlockByNumber" => Ok({
                    let mut block = Self::block();
                    block["baseFeePerGas"] = json!("0x4c4b40");
                    block
                }),
                "eth_maxPriorityFeePerGas" => Ok(json!("0x989680")),
                "eth_getTransactionCount" => Ok(json!("0x1")),
                "eth_getBalance" => Ok(json!(hex_quantity(self.balance))),
                "eth_estimateGas" => Ok(json!("0x5208")),
                "eth_blockNumber" => Ok(json!("0x10")),
                "eth_getTransactionReceipt" => Ok(self.receipt.lock().unwrap().clone()),
                "eth_getTransactionByHash" => {
                    let tx = self.tx.lock().unwrap().clone().unwrap();
                    let receipt = self.receipt.lock().unwrap();
                    Ok(
                        json!({"hash":receipt["transactionHash"], "blockHash":receipt["blockHash"], "blockNumber":receipt["blockNumber"],
                        "from":tx.sender, "to":tx.to, "chainId":"0x2105","type":"0x2", "nonce":format!("{:#x}",tx.nonce),
                        "gas":format!("{:#x}",tx.gas), "value":hex_quantity(u256(&tx.value)?), "input":tx.calldata,
                        "maxFeePerGas":hex_quantity(u256(&tx.max_fee_per_gas)?), "maxPriorityFeePerGas":hex_quantity(u256(&tx.max_priority_fee_per_gas)?),
                        "accessList":[]}),
                    )
                }
                "eth_call" => {
                    let to = params[0]["to"].as_str().unwrap();
                    if to != ORACLE && to != ATTRIBUTES {
                        return Ok(json!("0x"));
                    }
                    assert_eq!(
                        params[1],
                        json!({"blockHash":Self::block()["hash"],"requireCanonical":true})
                    );
                    let bytes = rpc_bytes(&params[0]["data"])?;
                    let selector = &bytes[..4];
                    let output = if selector == isFjordCall::SELECTOR
                        || selector == isIsthmusCall::SELECTOR
                    {
                        U256::from(1)
                    } else if selector == isJovianCall::SELECTOR {
                        U256::from(u8::from(self.jovian))
                    } else if selector == getL1FeeUpperBoundCall::SELECTOR {
                        assert_eq!(to, ORACLE);
                        *self.l1.lock().unwrap()
                    } else if selector == getOperatorFeeCall::SELECTOR {
                        let input = getOperatorFeeCall::abi_decode(&bytes).unwrap();
                        operator_cost(
                            if self.jovian {
                                OperatorFeeModel::Jovian
                            } else {
                                OperatorFeeModel::Isthmus
                            },
                            input.gas.to::<u64>(),
                            self.scalar,
                            self.constant,
                        )
                    } else if selector == operatorFeeScalarCall::SELECTOR {
                        assert_eq!(to, ATTRIBUTES);
                        U256::from(self.scalar)
                    } else if selector == operatorFeeConstantCall::SELECTOR {
                        assert_eq!(to, ATTRIBUTES);
                        U256::from(self.constant)
                    } else {
                        panic!("Unexpected fee ABI");
                    };
                    Ok(json!(hex_bytes(&output.to_be_bytes::<32>())))
                }
                _ => panic!("Unexpected fixture method {method}"),
            }
        }
    }
    fn tx() -> FrozenTransaction {
        FrozenTransaction {
            chain_id: BASE,
            sender: "0x1111111111111111111111111111111111111111".into(),
            to: "0x3333333333333333333333333333333333333333".into(),
            value: quantity(U256::from(10)),
            calldata: "0x".into(),
            nonce: 1,
            gas: 25200,
            max_fee_per_gas: quantity(U256::from(30_000_000)),
            max_priority_fee_per_gas: quantity(U256::from(15_000_000)),
            rollup_fee: None,
        }
    }
    #[test]
    fn operator_vectors_follow_active_fork_with_integer_rounding() {
        assert_eq!(
            operator_cost(OperatorFeeModel::PreIsthmus, 21000, 32, 7),
            U256::ZERO
        );
        assert_eq!(
            operator_cost(OperatorFeeModel::Isthmus, 21000, 32, 7),
            U256::from(7)
        );
        assert_eq!(
            operator_cost(OperatorFeeModel::Jovian, 21000, 32, 7),
            U256::from(67_200_007)
        );
        assert!(operator_cost(OperatorFeeModel::Jovian, u64::MAX, u32::MAX, u64::MAX) < U256::MAX);
    }
    #[test]
    fn legacy_serialization_remains_absent_and_base_requires_evidence() {
        let mut transaction = tx();
        transaction.chain_id = 1;
        let json = serde_json::to_value(&transaction).unwrap();
        assert!(json.get("rollup_fee").is_none());
        let original: FrozenTransaction = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(serde_json::to_value(&original).unwrap(), json);
        assert_eq!(
            original.maximum_cost().unwrap(),
            U256::from(756_000_000_000_u64)
        );
        transaction.chain_id = BASE;
        assert_eq!(
            transaction.maximum_cost().unwrap_err().code,
            "ROLLUP_FEE_EVIDENCE_INVALID"
        );
    }
    #[tokio::test]
    async fn prepare_reserves_all_fees_within_total_cap_and_revalidates() {
        let rpc = Arc::new(Fixture::new());
        let runtime = EvmRuntime::new(rpc.clone(), BASE, None);
        let cap = U256::from(504_004_000_000_u64);
        let prepared = runtime
            .prepare(
                address(&tx().sender).unwrap(),
                address(&tx().to).unwrap(),
                U256::from(10),
                Bytes::new(),
                cap,
            )
            .await
            .unwrap();
        assert!(prepared.transaction.maximum_cost().unwrap() <= cap);
        let evidence = prepared.transaction.rollup_fee.as_ref().unwrap();
        assert_eq!(evidence.maximum_l1_data_fee, quantity(U256::from(150)));
        assert_eq!(
            evidence.maximum_operator_fee,
            quantity(U256::from(3_780_011))
        );
        runtime.revalidate(&prepared.transaction).await.unwrap();
        *rpc.l1.lock().unwrap() = U256::from(120);
        runtime.revalidate(&prepared.transaction).await.unwrap();
        *rpc.l1.lock().unwrap() = U256::from(151);
        assert_eq!(
            runtime
                .revalidate(&prepared.transaction)
                .await
                .unwrap_err()
                .code,
            "FEE_BOUND_CHANGED"
        );
    }
    #[tokio::test]
    async fn metadata_rejects_wrong_chain_version_or_unsigned_size() {
        let runtime = EvmRuntime::new(Arc::new(Fixture::new()), BASE, None);
        let mut transaction = tx();
        transaction.rollup_fee = runtime.rollup_fee_bound(&transaction).await.unwrap();
        assert!(transaction.maximum_cost().is_ok());
        transaction.rollup_fee.as_mut().unwrap().version = 2;
        assert!(transaction.maximum_cost().is_err());
        transaction.rollup_fee.as_mut().unwrap().version = 1;
        transaction.calldata = "0x12345678".into();
        assert!(transaction.maximum_cost().is_err());
        transaction.calldata = "0x".into();
        transaction.chain_id = 1;
        assert!(transaction.maximum_cost().is_err());
    }
    #[tokio::test]
    async fn prepare_refuses_balance_covering_only_execution() {
        let mut fixture = Fixture::new();
        fixture.balance = U256::from(756_000_000_010_u64);
        let runtime = EvmRuntime::new(Arc::new(fixture), BASE, None);
        let failure = runtime
            .prepare(
                address(&tx().sender).unwrap(),
                address(&tx().to).unwrap(),
                U256::from(10),
                Bytes::new(),
                U256::from(1_000_000_000_000_u64),
            )
            .await
            .unwrap_err();
        assert_eq!(failure.code, "INSUFFICIENT_GAS_BALANCE");
    }
    #[tokio::test]
    async fn canonical_revert_records_actual_rollup_fees_even_over_reservation() {
        let rpc = Arc::new(Fixture::new());
        let runtime = EvmRuntime::new(rpc.clone(), BASE, None);
        let mut transaction = tx();
        transaction.rollup_fee = runtime.rollup_fee_bound(&transaction).await.unwrap();
        *rpc.tx.lock().unwrap() = Some(transaction.clone());
        let tx_hash = format!("0x{}", "11".repeat(32));
        *rpc.receipt.lock().unwrap() = json!({"transactionHash":tx_hash,"blockHash":Fixture::block()["hash"],"blockNumber":"0x10", "status":"0x0", "gasUsed":"0x5208","effectiveGasPrice":"0x2", "l1Fee":"0xf4240", "operatorFeeScalar":"0x1","operatorFeeConstant":"0x7"});
        let submission = Submission {
            kind: "ethereum".into(),
            reference: tx_hash,
            data: json!({}),
        };
        let receipt = runtime
            .receipt(&submission, &transaction, 1)
            .await
            .unwrap()
            .unwrap();
        assert!(!receipt.success);
        assert_eq!(receipt.fee, U256::from(3_142_007));
        // L1 actual exceeded the 150-wei reservation: retain the actual cost.
        assert!(receipt.fee > u256(&transaction.rollup_fee.unwrap().maximum_l1_data_fee).unwrap());
    }
    #[tokio::test]
    async fn omitted_zero_operator_fields_require_verified_parameters() {
        let mut fixture = Fixture::new();
        fixture.scalar = 0;
        fixture.constant = 0;
        let runtime = EvmRuntime::new(Arc::new(fixture), BASE, None);
        let receipt = json!({"l1Fee":"0x64"});
        let reported = json!({"type":"0x2","chainId":"0x2105"});
        assert_eq!(
            runtime
                .receipt_total_fee(&receipt, &reported, &Fixture::block(), 21000, U256::from(2))
                .await
                .unwrap(),
            U256::from(42100)
        );
        let runtime = EvmRuntime::new(Arc::new(Fixture::new()), BASE, None);
        assert!(
            runtime
                .receipt_total_fee(&receipt, &reported, &Fixture::block(), 21000, U256::from(2))
                .await
                .is_err()
        );
        assert!(
            runtime
                .receipt_total_fee(
                    &json!({}),
                    &reported,
                    &Fixture::block(),
                    21000,
                    U256::from(2)
                )
                .await
                .is_err()
        );
        assert!(
            runtime
                .receipt_total_fee(
                    &json!({"l1Fee":"0x64","operatorFeeScalar":"0x2","operatorFeeConstant":"0x7"}),
                    &reported,
                    &Fixture::block(),
                    21000,
                    U256::from(2)
                )
                .await
                .is_err()
        );
    }
    #[tokio::test]
    async fn unsupported_header_fee_semantics_and_charged_deposits_fail_closed() {
        let runtime = EvmRuntime::new(Arc::new(Fixture::new()), BASE, None);
        let mut block = Fixture::block();
        block["extraData"] = json!("0x02");
        assert_eq!(
            runtime.fee_model(&block).await.unwrap_err().code,
            "ROLLUP_FEE_EVIDENCE_INVALID"
        );
        assert_eq!(
            runtime
                .receipt_total_fee(
                    &json!({}),
                    &json!({"type":"0x7e"}),
                    &Fixture::block(),
                    21000,
                    U256::ZERO
                )
                .await
                .unwrap(),
            U256::ZERO
        );
        assert!(
            runtime
                .receipt_total_fee(
                    &json!({}),
                    &json!({"type":"0x7e"}),
                    &Fixture::block(),
                    21000,
                    U256::from(1)
                )
                .await
                .is_err()
        );
    }
}
