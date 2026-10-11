//! Reviewed single-pool Uniswap route primitives. Protocol-specific ABI and
//! route validation live in separate modules so additional AMMs can reuse the
//! generic bounds in `fin-amm` without inheriting Uniswap assumptions.
pub(crate) mod profile;
pub mod ramses;
pub mod v3;
pub mod v4;

use super::*;

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Protocol {
    #[default]
    V3,
    V4,
}

impl Protocol {
    pub fn parse(value: Option<&str>) -> Result<Self> {
        match value.unwrap_or("v3") {
            "v3" => Ok(Self::V3),
            "v4" => Ok(Self::V4),
            _ => Err(error(
                "UNISWAP_VERSION_UNSUPPORTED",
                "validation",
                "Select configured Uniswap v3 or v4.",
            )),
        }
    }
}

pub fn as_amm_quote(
    adapter: &EthereumAdapter,
    resource: ResourceId,
    quote: &reads::SwapQuote,
    maximum_fee: Quantity,
    permission: fin_amm::PermissionRequirement,
) -> Result<fin_amm::AmmQuote> {
    let dex_id = if quote.protocol == "v4" {
        "uniswap"
    } else {
        adapter
            .v3_profile(if quote.dex.is_empty() {
                "uniswap"
            } else {
                &quote.dex
            })?
            .dex_id
    };
    let router = if quote.protocol == "v4" {
        adapter
            .uniswap()?
            .v4
            .as_ref()
            .ok_or_else(|| {
                error(
                    "OPERATION_UNAVAILABLE",
                    "validation",
                    "Uniswap v4 is not configured.",
                )
            })?
            .router
            .clone()
    } else {
        adapter.v3_profile(dex_id)?.router.to_owned()
    };
    let mut route_data = json!({"chain_id":adapter.config.chain_id,"router":router,"protocol":quote.protocol,"pool":quote.pool,"fee":quote.fee_tier,"pool_key":quote.pool_key,"token_in":quote.pool_token_in,"token_out":quote.pool_token_out});
    if quote.dex == "ramses" {
        route_data["dex"] = serde_json::json!(dex_id);
        route_data["tick_spacing"] = serde_json::json!(quote.tick_spacing);
        route_data
            .as_object_mut()
            .expect("route identity object")
            .remove("fee");
    }
    if profile::additional_dex(dex_id).is_some() {
        route_data["dex"] = json!(dex_id);
        route_data["factory"] = json!(adapter.v3_profile(dex_id)?.factory);
    }
    let route = fin_amm::RouteIdentity {
        dex: dex_id.into(),
        version: quote.protocol.clone(),
        assets: vec![
            quote.token_in.asset(adapter),
            quote.token_out.asset(adapter),
        ],
        fingerprint: fin_domain::binding_hash(&route_data)?,
    };
    Ok(fin_amm::AmmQuote {
        resource,
        route,
        input: Amount {
            asset: quote.token_in.asset(adapter),
            atoms: quote.input.clone(),
        },
        output: Amount {
            asset: quote.token_out.asset(adapter),
            atoms: quote.output.clone(),
        },
        maximum_fee: Amount {
            asset: adapter.native_asset(),
            atoms: maximum_fee,
        },
        price_impact_bps: quote.price_impact_bps,
        permission,
        evidence: fin_amm::QuoteEvidence {
            observed_at: quote.observed_at.clone(),
            block_number: quote.block_number,
            block_hash: quote.block_hash.clone(),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::abi::{SwapRouter02, UniversalRouter};
    use fin_evm::alloy::{
        primitives::{
            Bytes, U256,
            aliases::{U48, U160},
        },
        sol_types::{SolCall, SolValue},
    };

    fn key() -> V4PoolKey {
        V4PoolKey {
            currency0: format!("{:#x}", Address::ZERO),
            currency1: "0x0000000000000000000000000000000000000001".into(),
            fee: 500,
            tick_spacing: 10,
            hooks: format!("{:#x}", Address::ZERO),
        }
    }

    #[test]
    fn v4_pool_key_and_native_directions_are_strictly_bound() {
        let key = key();
        let router = address("0x0000000000000000000000000000000000000002").unwrap();
        let wallet = address("0x0000000000000000000000000000000000000003").unwrap();
        let (calldata, value) = v4::exact_input_single(v4::ExactInputSingle {
            key: &key,
            input_asset: Address::ZERO,
            output_asset: address(&key.currency1).unwrap(),
            wallet,
            amount_in: U256::from(10),
            minimum: U256::from(9),
            deadline: 100,
            has_hop_price: true,
            native_in: true,
        })
        .unwrap();
        assert_eq!(&calldata[..4], &UniversalRouter::executeCall::SELECTOR);
        assert_eq!(value, U256::from(10));
        let (reverse, reverse_value) = v4::exact_input_single(v4::ExactInputSingle {
            key: &key,
            input_asset: address(&key.currency1).unwrap(),
            output_asset: Address::ZERO,
            wallet,
            amount_in: U256::from(10),
            minimum: U256::from(9),
            deadline: 100,
            has_hop_price: true,
            native_in: false,
        })
        .unwrap();
        assert_eq!(&reverse[..4], &UniversalRouter::executeCall::SELECTOR);
        assert_eq!(reverse_value, U256::ZERO);
        let mut hooked = key.clone();
        hooked.hooks = format!("{router:#x}");
        assert_eq!(
            v4::pool_key(&hooked).err().unwrap().code,
            "V4_POOL_UNSUPPORTED"
        );
        let mut unsorted = key;
        unsorted.currency0 = "0x0000000000000000000000000000000000000002".into();
        assert_eq!(
            v4::pool_key(&unsorted).err().unwrap().code,
            "V4_POOL_UNSUPPORTED"
        );
    }

    #[test]
    fn v3_router02_uses_deadline_multicall_and_native_output_unwrap() {
        let router = address("0x0000000000000000000000000000000000000001").unwrap();
        let wallet = address("0x0000000000000000000000000000000000000002").unwrap();
        let call = v3::exact_input_single(v3::ExactInputSingle {
            kind: RouterKind::SwapRouter02,
            router,
            token_in: address("0x0000000000000000000000000000000000000003").unwrap(),
            token_out: address("0x0000000000000000000000000000000000000004").unwrap(),
            fee: 500,
            wallet,
            deadline: 123,
            input: U256::from(10),
            minimum: U256::from(9),
            native_in: false,
            native_out: true,
        })
        .unwrap();
        assert_eq!(&call.calldata[..4], &SwapRouter02::multicallCall::SELECTOR);
        assert_eq!(call.value, U256::ZERO);
        let decoded = SwapRouter02::multicallCall::abi_decode(&call.calldata).unwrap();
        assert_eq!(decoded.deadline, U256::from(123));
        assert_eq!(decoded.data.len(), 2);
    }

    #[test]
    fn ramses_v3_selector_and_int24_spacing_are_distinct_from_uniswap_fee_tier() {
        use crate::abi::{RamsesRouter, Router};
        use fin_evm::alloy::primitives::aliases::I24;
        let router = address("0x0000000000000000000000000000000000000001").unwrap();
        let wallet = address("0x0000000000000000000000000000000000000002").unwrap();
        let request = ramses::ExactInputSingle {
            router,
            token_in: address("0x0000000000000000000000000000000000000003").unwrap(),
            token_out: address("0x0000000000000000000000000000000000000004").unwrap(),
            tick_spacing: 1,
            wallet,
            deadline: 123,
            input: U256::from(10),
            minimum: U256::from(9),
            native_in: false,
            native_out: false,
        };
        let call = ramses::exact_input_single(request).unwrap();
        assert_ne!(
            RamsesRouter::exactInputSingleCall::SELECTOR,
            Router::exactInputSingleCall::SELECTOR,
            "int24 tickSpacing produces a different function selector from uint24 fee"
        );
        let decoded = RamsesRouter::exactInputSingleCall::abi_decode(&call.calldata).unwrap();
        assert_eq!(decoded.params.tickSpacing, I24::try_from(1i64).unwrap());
        assert_eq!(decoded.params.amountOutMinimum, U256::from(9));
        assert_eq!(call.value, U256::ZERO);
        let native_out = ramses::exact_input_single(ramses::ExactInputSingle {
            native_out: true,
            ..request
        })
        .unwrap();
        assert_eq!(
            &native_out.calldata[..4],
            &RamsesRouter::multicallCall::SELECTOR
        );
        assert_eq!(
            RamsesRouter::multicallCall::abi_decode(&native_out.calldata)
                .unwrap()
                .data
                .len(),
            2
        );
    }

    #[test]
    fn typed_permits_are_encoded_only_as_zeroed_v3_v4_signature_slots() {
        use fin_evm::{Permit2PermitSingle, SignatureLayout};
        let token = address("0x0000000000000000000000000000000000000003").unwrap();
        let router = address("0x0000000000000000000000000000000000000004").unwrap();
        let wallet = address("0x0000000000000000000000000000000000000005").unwrap();
        let v3_call = v3::exact_input_single(v3::ExactInputSingle {
            kind: RouterKind::SwapRouter02,
            router,
            token_in: token,
            token_out: address("0x0000000000000000000000000000000000000006").unwrap(),
            fee: 500,
            wallet,
            deadline: 123,
            input: U256::from(10),
            minimum: U256::from(9),
            native_in: false,
            native_out: false,
        })
        .unwrap();
        let (encoded_v3, layout) = v3::with_eip2612(
            RouterKind::SwapRouter02,
            v3_call.calldata,
            token,
            U256::from(10),
            123,
        )
        .unwrap();
        let SignatureLayout::Eip2612Abi {
            v_offset,
            r_offset,
            s_offset,
        } = layout
        else {
            panic!("EIP-2612 must use ABI-word offsets")
        };
        for offset in [v_offset, r_offset, s_offset] {
            assert_eq!(&encoded_v3[offset..offset + 32], &[0; 32]);
        }
        let decoded = SwapRouter02::multicallCall::abi_decode(&encoded_v3).unwrap();
        assert_eq!(decoded.data.len(), 2);
        assert_eq!(
            &decoded.data[0][..4],
            &SwapRouter02::selfPermitCall::SELECTOR
        );
        let permit_call = SwapRouter02::selfPermitCall::abi_decode(&decoded.data[0]).unwrap();
        assert_eq!(permit_call.token, token);
        assert_eq!(permit_call.value, U256::from(10));
        assert_eq!(permit_call.deadline, U256::from(123));
        assert_eq!(permit_call.v, 0);
        assert_eq!(permit_call.r, fin_evm::alloy::primitives::B256::ZERO);
        assert_eq!(permit_call.s, fin_evm::alloy::primitives::B256::ZERO);

        let pool = key();
        let pool_token = address(&pool.currency1).unwrap();
        let base = v4::exact_input_single(v4::ExactInputSingle {
            key: &pool,
            input_asset: pool_token,
            output_asset: Address::ZERO,
            wallet,
            amount_in: U256::from(10),
            minimum: U256::from(9),
            deadline: 123,
            has_hop_price: true,
            native_in: false,
        })
        .unwrap()
        .0;
        let permit = Permit2PermitSingle {
            chain_id: 1,
            verifying_contract: "0x000000000022d473030f116ddee9f6b43ac78ba3".into(),
            owner: format!("{wallet:#x}"),
            token: format!("{pool_token:#x}"),
            amount: Quantity::new("10").unwrap(),
            expiration: 123,
            nonce: 7,
            spender: format!("{router:#x}"),
            sig_deadline: 123,
        };
        let (encoded_v4, offset) = v4::with_permit2(base, &permit).unwrap();
        assert_eq!(&encoded_v4[offset..offset + 65], &[0; 65]);
        let decoded = UniversalRouter::executeCall::abi_decode(&encoded_v4).unwrap();
        assert_eq!(decoded.commands.as_ref(), [v4::PERMIT2_PERMIT, v4::V4_SWAP]);
        type PermitSingle = ((Address, U160, U48, U48), Address, U256);
        let permit_words = decoded.inputs[0].as_ref();
        let word = |index: usize| &permit_words[index * 32..(index + 1) * 32];
        assert_eq!(&word(0)[12..], pool_token.as_slice());
        assert_eq!(U256::from_be_slice(word(6)), U256::from(224));
        let (decoded_single, signature): (PermitSingle, Bytes) =
            <(PermitSingle, Bytes)>::abi_decode_params(permit_words).unwrap();
        let (decoded_details, decoded_spender, decoded_deadline) = decoded_single;
        assert_eq!(decoded_details.0, pool_token);
        assert_eq!(decoded_details.1, U160::from(10));
        assert_eq!(decoded_details.2, U48::from(123));
        assert_eq!(decoded_details.3, U48::from(7));
        assert_eq!(decoded_spender, router);
        assert_eq!(decoded_deadline, U256::from(123));
        assert_eq!(signature.as_ref(), &[0; 65]);
    }

    #[test]
    fn pool_id_is_keccak_of_the_sorted_static_pool_key() {
        let key = key();
        let id = v4::pool_id(&key).unwrap();
        let pool = v4::pool_key(&key).unwrap();
        let encoding = UniversalRouter::encodeV4SingleLegacyCall {
            poolKey: pool,
            zeroForOne: true,
            amountIn: 1,
            amountOutMinimum: 1,
            hookData: Bytes::new(),
        }
        .abi_encode();
        let hash = fin_evm::alloy::primitives::keccak256(&encoding[4..164]);
        assert_eq!(id, format!("{hash:#x}"));
    }
}
