use super::*;
use crate::abi::*;
use gofin_evm::SignatureLayout;
use gofin_evm::alloy::{
    primitives::{Address, B256, Bytes, U256, aliases::U24},
    sol_types::SolCall,
};

pub struct Call {
    pub calldata: Bytes,
    pub value: U256,
}

/// Wrap a reviewed v3 exact-input call in an atomic EIP-2612 `selfPermit`
/// followed by the swap. Signature words are distinct markers during ABI
/// encoding, then zeroed and frozen as explicit ABI-word offsets.
pub fn with_eip2612(
    kind: RouterKind,
    calldata: Bytes,
    token: Address,
    value: U256,
    deadline: u64,
) -> Result<(Bytes, SignatureLayout)> {
    let permit = match kind {
        RouterKind::LegacyV3 => Router::selfPermitCall {
            token,
            value,
            deadline: U256::from(deadline),
            v: 0xa5,
            r: B256::repeat_byte(0x22),
            s: B256::repeat_byte(0x33),
        }
        .abi_encode(),
        RouterKind::SwapRouter02 => SwapRouter02::selfPermitCall {
            token,
            value,
            deadline: U256::from(deadline),
            v: 0xa5,
            r: B256::repeat_byte(0x22),
            s: B256::repeat_byte(0x33),
        }
        .abi_encode(),
    };
    let (mut encoded, outer_deadline) = match kind {
        RouterKind::LegacyV3 => {
            let mut calls =
                if calldata.get(..4) == Some(&LegacyRouterPayments::multicallCall::SELECTOR) {
                    LegacyRouterPayments::multicallCall::abi_decode(&calldata)
                        .map_err(|_| {
                            error(
                                "INVALID_PERMIT_TEMPLATE",
                                "preview",
                                "Malformed Legacy Router multicall.",
                            )
                        })?
                        .data
                } else {
                    vec![calldata]
                };
            calls.insert(0, permit.into());
            (
                LegacyRouterPayments::multicallCall { data: calls }.abi_encode(),
                0,
            )
        }
        RouterKind::SwapRouter02 => {
            let decoded = SwapRouter02::multicallCall::abi_decode(&calldata).map_err(|_| {
                error(
                    "INVALID_PERMIT_TEMPLATE",
                    "preview",
                    "Router02 calldata must be a deadline multicall.",
                )
            })?;
            let mut calls: Vec<Bytes> = decoded.data;
            calls.insert(0, permit.into());
            (
                SwapRouter02::multicallCall {
                    deadline: decoded.deadline,
                    data: calls,
                }
                .abi_encode(),
                0,
            )
        }
    };
    let _ = outer_deadline;
    let locate = |needle: &[u8]| -> Result<usize> {
        let positions: Vec<_> = encoded
            .windows(needle.len())
            .enumerate()
            .filter_map(|(i, w)| (w == needle).then_some(i))
            .collect();
        if positions.len() != 1 {
            return Err(error(
                "INVALID_PERMIT_TEMPLATE",
                "preview",
                "EIP-2612 ABI signature slot is ambiguous.",
            ));
        }
        Ok(positions[0])
    };
    let r_marker = [0x22u8; 32];
    let s_marker = [0x33u8; 32];
    let r_offset = locate(&r_marker)?;
    let s_offset = locate(&s_marker)?;
    let v_offset = locate(&[0u8; 31].into_iter().chain([0xa5]).collect::<Vec<_>>())?;
    for offset in [v_offset, r_offset, s_offset] {
        encoded[offset..offset + 32].fill(0);
    }
    Ok((
        encoded.into(),
        SignatureLayout::Eip2612Abi {
            v_offset,
            r_offset,
            s_offset,
        },
    ))
}

#[derive(Clone, Copy, Debug)]
pub struct ExactInputSingle {
    pub kind: RouterKind,
    pub router: Address,
    pub token_in: Address,
    pub token_out: Address,
    pub fee: u32,
    pub wallet: Address,
    pub deadline: u64,
    pub input: U256,
    pub minimum: U256,
    pub native_in: bool,
    pub native_out: bool,
}

pub fn exact_input_single(request: ExactInputSingle) -> Result<Call> {
    let ExactInputSingle {
        kind,
        router,
        token_in,
        token_out,
        fee,
        wallet,
        deadline,
        input,
        minimum,
        native_in,
        native_out,
    } = request;
    if router == Address::ZERO
        || token_in == Address::ZERO
        || token_out == Address::ZERO
        || token_in == token_out
        || wallet == Address::ZERO
        || input.is_zero()
        || minimum.is_zero()
        || !(1..=10_000).contains(&fee)
        || deadline == 0
    {
        return Err(error(
            "SWAP_ROUTE_INVALID",
            "validation",
            "The v3 single-pool route has an invalid token, fee, amount, deadline or recipient.",
        ));
    }
    let inner = match kind {
        RouterKind::LegacyV3 => Router::exactInputSingleCall {
            params: Router::SwapParams {
                tokenIn: token_in,
                tokenOut: token_out,
                fee: U24::from(fee),
                recipient: if native_out { router } else { wallet },
                deadline: U256::from(deadline),
                amountIn: input,
                amountOutMinimum: minimum,
                sqrtPriceLimitX96: Default::default(),
            },
        }
        .abi_encode(),
        RouterKind::SwapRouter02 => SwapRouter02::exactInputSingleCall {
            params: SwapRouter02::SwapParams {
                tokenIn: token_in,
                tokenOut: token_out,
                fee: U24::from(fee),
                recipient: if native_out { router } else { wallet },
                amountIn: input,
                amountOutMinimum: minimum,
                sqrtPriceLimitX96: Default::default(),
            },
        }
        .abi_encode(),
    };
    let calldata = if native_out {
        let unwrap = match kind {
            RouterKind::LegacyV3 => LegacyRouterPayments::unwrapWETH9Call {
                amountMinimum: minimum,
                recipient: wallet,
            }
            .abi_encode(),
            RouterKind::SwapRouter02 => SwapRouter02::unwrapWETH9Call {
                amountMinimum: minimum,
                recipient: wallet,
            }
            .abi_encode(),
        };
        match kind {
            RouterKind::LegacyV3 => LegacyRouterPayments::multicallCall {
                data: vec![inner.into(), unwrap.into()],
            }
            .abi_encode(),
            RouterKind::SwapRouter02 => SwapRouter02::multicallCall {
                deadline: U256::from(deadline),
                data: vec![inner.into(), unwrap.into()],
            }
            .abi_encode(),
        }
    } else if matches!(kind, RouterKind::SwapRouter02) {
        SwapRouter02::multicallCall {
            deadline: U256::from(deadline),
            data: vec![inner.into()],
        }
        .abi_encode()
    } else {
        inner
    };
    Ok(Call {
        calldata: calldata.into(),
        value: if native_in { input } else { U256::ZERO },
    })
}

/// Pancake SmartRouter requires a final native refund step. The approved
/// exact-input call remains first, with its original deadline unchanged.
pub(crate) fn with_refund(calldata: Bytes) -> Result<Bytes> {
    let mut call = SwapRouter02::multicallCall::abi_decode(&calldata).map_err(|_| {
        error(
            "SWAP_ROUTE_INVALID",
            "validation",
            "SmartRouter requires a deadline multicall.",
        )
    })?;
    call.data
        .push(V3Refund::refundETHCall {}.abi_encode().into());
    Ok(call.abi_encode().into())
}
