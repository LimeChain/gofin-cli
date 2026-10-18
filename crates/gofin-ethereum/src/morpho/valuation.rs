//! Share allowances disclose the value of their underlying claim without
//! requiring or double-counting a separate market price for the receipt token.
use crate::*;
use gofin_evm::alloy::sol;

sol! {
    interface ShareValuation {
        function isVaultV2(address vault) external view returns (bool);
        function previewRedeem(uint256 shares) external view returns (uint256);
    }
}

impl EthereumAdapter {
    pub(crate) async fn morpho_allowance_value(
        &self,
        token: &AssetId,
        spender: Option<&str>,
        shares: U256,
    ) -> Result<Option<(AssetId, Quantity)>> {
        let Ok(profile) = super::profile::profile(self.config.chain_id) else {
            return Ok(None);
        };
        if spender.map(address).transpose()? != Some(address(profile.vault_bundles)?) {
            return Ok(None);
        }
        let vault = address(&token.reference)?;
        if !self
            .runtime
            .call(
                address(profile.vault_factory)?,
                &ShareValuation::isVaultV2Call { vault },
                None,
            )
            .await?
        {
            return Ok(None);
        }
        let state = self
            .morpho_vault_state(&token.reference, None, None)
            .await?;
        Ok(Some(
            allowance_value_at(&self.runtime, token, shares, &state).await?,
        ))
    }
}

// The only production caller supplies a factory/code-verified state above.
// Keep conversion pinned to its block, separate from metadata verification.
async fn allowance_value_at(
    runtime: &EvmRuntime,
    token: &AssetId,
    shares: U256,
    state: &super::reads::MorphoVaultState,
) -> Result<(AssetId, Quantity)> {
    require(
        state.shares == *token,
        "MORPHO_SHARE_IDENTITY",
        "Allowance share metadata differs from the verified vault.",
    )?;
    let assets = runtime
        .call(
            address(&state.vault)?,
            &ShareValuation::previewRedeemCall { shares },
            Some(state.block),
        )
        .await?;
    Ok((state.asset.clone(), quantity(assets)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gofin_evm::{
        EvmRpc,
        alloy::{
            primitives::B256,
            sol_types::{SolCall, SolValue},
        },
    };
    use std::sync::Mutex;

    #[derive(Default)]
    struct ProbeRpc {
        calls: Mutex<Vec<(String, Value)>>,
    }
    #[async_trait]
    impl EvmRpc for ProbeRpc {
        async fn request(&self, method: &str, params: Value) -> Result<Value> {
            self.calls
                .lock()
                .unwrap()
                .push((method.into(), params.clone()));
            Ok(match method {
                "eth_chainId" => json!("0x1"),
                "eth_blockNumber" => json!("0x14"),
                "eth_getBlockByNumber" => {
                    json!({"hash":format!("{:#x}",B256::repeat_byte(0xbb)),"number":"0x14","timestamp":format!("{:#x}",chrono::Utc::now().timestamp())})
                }
                "eth_call" => {
                    let bytes = gofin_evm::rpc_bytes(&params[0]["data"])?;
                    if bytes.starts_with(&ShareValuation::isVaultV2Call::SELECTOR) {
                        json!(gofin_evm::hex_bytes(&false.abi_encode()))
                    } else if bytes.starts_with(&ShareValuation::previewRedeemCall::SELECTOR) {
                        let call =
                            ShareValuation::previewRedeemCall::abi_decode_validate(&bytes).unwrap();
                        // A share is backed by 1.25 USDC, never independently priced.
                        let assets = call.shares * U256::from(1_250_000) / pow10(18)?;
                        json!(gofin_evm::hex_bytes(&assets.abi_encode()))
                    } else {
                        return Err(GoFinError::validation("Unexpected synthetic call."));
                    }
                }
                _ => return Err(GoFinError::validation("Unexpected synthetic RPC method.")),
            })
        }
    }
    fn state() -> super::super::reads::MorphoVaultState {
        serde_json::from_value(json!({"vault":lower_address(Address::repeat_byte(3)),"asset":{"network":"ethereum:1","reference":lower_address(Address::repeat_byte(2)),"decimals":6},"shares":{"network":"ethereum:1","reference":lower_address(Address::repeat_byte(3)),"decimals":18},"block":20,"block_hash":format!("{:#x}",B256::repeat_byte(0xbb)),"block_timestamp":1_700_000_000,"total_assets":"1250000","total_supply":"1000000000000000000","virtual_shares":"1000000000000","owner_shares":"0","owner_assets":"0","idle_assets":"1250000","liquidity_adapter":lower_address(Address::ZERO),"code_identity":"verified-fixture","profile_identity":"reviewed-fixture","gates":{},"adapters":[],"write_compatibility":{"supported":true},"capacity":{},"fees":{}})).unwrap()
    }
    fn adapter(rpc: Arc<ProbeRpc>) -> EthereumAdapter {
        EthereumAdapter::public_morpho(
            EthereumConfig::wallet_only(1, 1, vec![]),
            Arc::new(EvmRuntime::new(rpc, 1, None)),
        )
        .unwrap()
    }
    struct UnderlyingPrices {
        underlying: AssetId,
        calls: Mutex<Vec<AssetId>>,
    }
    #[async_trait]
    impl WalletValuation for UnderlyingPrices {
        async fn quote_usd(&self, asset: &AssetId, units: Quantity) -> Result<(Quantity, String)> {
            self.calls.lock().unwrap().push(asset.clone());
            let price = if asset == &self.underlying {
                Quantity::new("1000000")?
            } else if asset.reference == "native" {
                Quantity::new("2000000000")?
            } else {
                return Err(GoFinError::validation(
                    "Synthetic provider has no share-token price.",
                ));
            };
            Ok((
                units.mul_div_ceil(&price, &quantity(pow10(asset.decimals)?))?,
                "synthetic verified underlying price".into(),
            ))
        }
    }
    #[tokio::test]
    async fn share_allowance_is_valued_through_pinned_underlying_backing() {
        let rpc = Arc::new(ProbeRpc::default());
        let state = state();
        let prices = Arc::new(UnderlyingPrices {
            underlying: state.asset.clone(),
            calls: Mutex::new(vec![]),
        });
        let adapter = adapter(rpc.clone()).with_wallet_valuation(prices.clone());
        let shares = U256::from(1_500_000_000_000_000_000u64);
        let (asset, assets) = allowance_value_at(&adapter.runtime, &state.shares, shares, &state)
            .await
            .unwrap();
        assert_eq!(asset, state.asset);
        assert_eq!(assets, Quantity::new("1875000").unwrap());
        let valuation = super::super::vault::valuation(
            &adapter,
            &asset,
            &assets,
            U256::from(1_000_000_000_000_000u64),
        )
        .await
        .unwrap();
        assert_eq!(
            valuation.movement_usd_micros,
            Quantity::new("1875000").unwrap()
        );
        assert_eq!(
            valuation.maximum_fee_usd_micros,
            Quantity::new("2000000").unwrap()
        );
        let calls = rpc.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "eth_call");
        assert_eq!(calls[0].1[1], "0x14");
        assert_eq!(calls[0].1[0]["to"], state.vault);
        let priced = prices.calls.lock().unwrap();
        assert_eq!(priced.len(), 2);
        assert!(priced.contains(&state.asset));
        assert!(!priced.contains(&state.shares));
    }
    #[tokio::test]
    async fn wrong_share_identity_is_rejected_before_conversion() {
        let rpc = Arc::new(ProbeRpc::default());
        let adapter = adapter(rpc.clone());
        let state = state();
        for token in [
            AssetId {
                decimals: 6,
                ..state.shares.clone()
            },
            AssetId {
                network: "ethereum:8453".into(),
                ..state.shares.clone()
            },
            state.asset.clone(),
        ] {
            assert_eq!(
                allowance_value_at(&adapter.runtime, &token, U256::from(1), &state)
                    .await
                    .unwrap_err()
                    .code,
                "MORPHO_SHARE_IDENTITY"
            );
        }
        assert!(rpc.calls.lock().unwrap().is_empty());
    }
    #[tokio::test]
    async fn unrelated_spenders_and_non_v2_tokens_keep_generic_approval_path() {
        let rpc = Arc::new(ProbeRpc::default());
        let adapter = adapter(rpc.clone());
        let state = state();
        let other = lower_address(Address::repeat_byte(9));
        assert!(
            adapter
                .morpho_allowance_value(&state.shares, Some(&other), U256::from(7))
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            adapter
                .morpho_allowance_value(&state.shares, None, U256::from(7))
                .await
                .unwrap()
                .is_none()
        );
        assert!(rpc.calls.lock().unwrap().is_empty());
        let profile = super::super::profile::profile(1).unwrap();
        assert!(
            adapter
                .morpho_allowance_value(&state.asset, Some(profile.vault_bundles), U256::from(7))
                .await
                .unwrap()
                .is_none()
        );
        let calls = rpc.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].1[0]["to"], profile.vault_factory);
        assert_eq!(calls[0].1[1], "latest");
        let data = gofin_evm::rpc_bytes(&calls[0].1[0]["data"]).unwrap();
        assert_eq!(
            ShareValuation::isVaultV2Call::abi_decode_validate(&data)
                .unwrap()
                .vault,
            address(&state.asset.reference).unwrap()
        );
    }
    #[tokio::test]
    async fn public_adapter_has_no_wallet_authority_or_fabricated_holdings() {
        let rpc = Arc::new(ProbeRpc::default());
        let adapter = adapter(rpc.clone());
        assert!(adapter.supports_operation("morpho.vaults"));
        assert!(adapter.supports_operation("morpho.vault"));
        for op in [
            "morpho.deposit",
            "morpho.withdraw",
            "morpho.redeem",
            "morpho.rewards.claim",
            "wallet.approve",
            "wallet.transfer",
        ] {
            assert!(!adapter.supports_operation(op));
        }
        let snapshot = adapter.read_snapshot().await.unwrap();
        assert!(snapshot.complete);
        assert!(
            snapshot.state.holdings.is_empty()
                && snapshot.state.positions.is_empty()
                && snapshot.state.authorities.is_empty()
        );
        assert!(snapshot.coverage.is_empty());
        assert!(
            rpc.calls
                .lock()
                .unwrap()
                .iter()
                .all(|(method, _)| method != "eth_call")
        );
    }
}
