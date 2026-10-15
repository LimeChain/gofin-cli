use super::*;
use crate::abi::*;
use gofin_evm::alloy::{
    primitives::{Address, Bytes, U256},
    sol_types::SolCall,
};

pub struct Call {
    pub calldata: Bytes,
    pub value: U256,
}

#[derive(Clone, Copy)]
pub struct ExactInputSingle {
    pub router: Address,
    pub token_in: Address,
    pub token_out: Address,
    pub tick_spacing: i32,
    pub wallet: Address,
    pub deadline: u64,
    pub input: U256,
    pub minimum: U256,
    pub native_in: bool,
    pub native_out: bool,
}

pub fn exact_input_single(request: ExactInputSingle) -> Result<Call> {
    let ExactInputSingle {
        router,
        token_in,
        token_out,
        tick_spacing,
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
        || ![1, 5, 10, 50, 100, 200].contains(&tick_spacing)
        || deadline == 0
    {
        return Err(error(
            "SWAP_ROUTE_INVALID",
            "validation",
            "The Ramses single-pool route has an invalid token, tick spacing, amount, deadline or recipient.",
        ));
    }
    let swap = RamsesRouter::exactInputSingleCall {
        params: RamsesRouter::SwapParams {
            tokenIn: token_in,
            tokenOut: token_out,
            tickSpacing: gofin_evm::alloy::primitives::aliases::I24::try_from(i64::from(
                tick_spacing,
            ))
            .expect("reviewed positive spacing"),
            recipient: if native_out { router } else { wallet },
            deadline: U256::from(deadline),
            amountIn: input,
            amountOutMinimum: minimum,
            sqrtPriceLimitX96: Default::default(),
        },
    }
    .abi_encode();
    let calldata = if native_out {
        let unwrap = RamsesRouter::unwrapWETH9Call {
            amountMinimum: minimum,
            recipient: wallet,
        }
        .abi_encode();
        RamsesRouter::multicallCall {
            data: vec![swap.into(), unwrap.into()],
        }
        .abi_encode()
    } else {
        swap
    };
    Ok(Call {
        calldata: calldata.into(),
        value: if native_in { input } else { U256::ZERO },
    })
}

pub fn with_eip2612(
    calldata: Bytes,
    token: Address,
    value: U256,
    deadline: u64,
) -> Result<(Bytes, gofin_evm::SignatureLayout)> {
    // Ramses deliberately shares Legacy Router's selfPermit and multicall
    // selectors and argument layout. Reuse its audited signature-slot logic;
    // keep the protocol-specific exactInputSingle encoding in this module.
    if RamsesRouter::selfPermitCall::SELECTOR != Router::selfPermitCall::SELECTOR
        || RamsesRouter::multicallCall::SELECTOR != LegacyRouterPayments::multicallCall::SELECTOR
    {
        return Err(error(
            "INVALID_PERMIT_TEMPLATE",
            "preview",
            "Ramses permit wrapper ABI does not match the reviewed Legacy Router layout.",
        ));
    }
    super::v3::with_eip2612(RouterKind::LegacyV3, calldata, token, value, deadline)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ramses_uses_spacing_selector_and_shared_legacy_permit_envelope() {
        assert_ne!(
            RamsesRouter::exactInputSingleCall::SELECTOR,
            Router::exactInputSingleCall::SELECTOR,
            "Ramses encodes int24 tick spacing where Uniswap encodes uint24 fee",
        );
        assert_eq!(
            RamsesRouter::selfPermitCall::SELECTOR,
            Router::selfPermitCall::SELECTOR,
        );
        assert_eq!(
            RamsesRouter::multicallCall::SELECTOR,
            LegacyRouterPayments::multicallCall::SELECTOR,
        );
    }
}
