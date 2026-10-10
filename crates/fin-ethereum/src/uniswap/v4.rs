use super::*;
use crate::abi::*;
use fin_evm::alloy::{
    primitives::{
        Bytes, U256,
        aliases::{I24, U24, U48, U160},
        keccak256,
    },
    sol_types::{SolCall, SolValue},
};
use fin_evm::u256;

pub const SWAP_EXACT_IN_SINGLE: u8 = 0x06;
pub const SETTLE_ALL: u8 = 0x0c;
pub const TAKE_ALL: u8 = 0x0f;
pub const V4_SWAP: u8 = 0x10;
pub const PERMIT2_PERMIT: u8 = 0x0a;

/// Add the reviewed Universal Router Permit2 command before a v4 swap and
/// return the location of its unique signature slot in the complete calldata.
pub fn with_permit2(
    calldata: Bytes,
    permit: &fin_evm::Permit2PermitSingle,
) -> Result<(Bytes, usize)> {
    let decoded = UniversalRouter::executeCall::abi_decode(&calldata).map_err(|_| {
        error(
            "INVALID_PERMIT_TEMPLATE",
            "preview",
            "Cannot extend malformed v4 router calldata.",
        )
    })?;
    if decoded.commands.as_ref() != [V4_SWAP] || decoded.inputs.len() != 1 {
        return Err(error(
            "INVALID_PERMIT_TEMPLATE",
            "preview",
            "Permit2 is supported only for a single reviewed v4 swap command.",
        ));
    }
    let marker = [0xa5u8; 65];
    let token = address(&permit.token)?;
    // Universal Router supplies msgSender as the Permit2 owner. The command
    // payload is therefore `(PermitSingle,bytes)`, with no encoded owner word.
    let owner = address(&permit.owner)?;
    let spender = address(&permit.spender)?;
    let amount_u256 = u256(&permit.amount)?;
    if amount_u256 > U256::from(U160::MAX) {
        return Err(error(
            "INVALID_PERMIT_TEMPLATE",
            "preview",
            "Permit2 amount exceeds uint160.",
        ));
    }
    let limbs = amount_u256.into_limbs();
    let amount = U160::from_limbs([limbs[0], limbs[1], limbs[2]]);
    let expiration = U48::from(permit.expiration);
    let nonce = U48::from(permit.nonce);
    let sig_deadline = U256::from(permit.sig_deadline);
    if owner == Address::ZERO {
        return Err(error(
            "INVALID_PERMIT_TEMPLATE",
            "preview",
            "Permit2 owner must be a nonzero wallet address.",
        ));
    }
    let permit_single = ((token, amount, expiration, nonce), spender, sig_deadline);
    // This is a flat argument list, not an ABI-encoded tuple value. `abi_encode`
    // wraps the dynamic tuple in a leading offset word, which shifts every
    // field and makes Universal Router's decoder revert.
    let permit_input = (permit_single, Bytes::copy_from_slice(&marker)).abi_encode_params();
    let mut commands = vec![PERMIT2_PERMIT];
    commands.extend_from_slice(decoded.commands.as_ref());
    let mut inputs = vec![Bytes::from(permit_input)];
    inputs.extend(decoded.inputs);
    let encoded = UniversalRouter::executeCall {
        commands: Bytes::from(commands),
        inputs,
        deadline: decoded.deadline,
    }
    .abi_encode();
    let positions: Vec<_> = encoded
        .windows(marker.len())
        .enumerate()
        .filter_map(|(i, window)| (window == marker).then_some(i))
        .collect();
    if positions.len() != 1 {
        return Err(error(
            "INVALID_PERMIT_TEMPLATE",
            "preview",
            "Permit2 signature slot is ambiguous.",
        ));
    }
    let signature_offset = positions[0];
    let mut encoded = encoded;
    encoded[signature_offset..signature_offset + 65].fill(0);
    Ok((Bytes::from(encoded), signature_offset))
}

pub fn pool_key(key: &V4PoolKey) -> Result<V4Quoter::PoolKey> {
    let c0 = address(&key.currency0)?;
    let c1 = address(&key.currency1)?;
    let hooks = address(&key.hooks)?;
    if c0 >= c1 || key.fee > 1_000_000 || key.tick_spacing <= 0 || hooks != Address::ZERO {
        return Err(error(
            "V4_POOL_UNSUPPORTED",
            "validation",
            "Uniswap v4 requires sorted currencies, finite fee, positive tick spacing and zero hooks.",
        ));
    }
    Ok(V4Quoter::PoolKey {
        currency0: c0,
        currency1: c1,
        fee: U24::from(key.fee),
        tickSpacing: I24::try_from(key.tick_spacing).map_err(|_| {
            error(
                "V4_POOL_UNSUPPORTED",
                "validation",
                "Tick spacing exceeds int24.",
            )
        })?,
        hooks,
    })
}

#[derive(Clone, Copy, Debug)]
pub struct ExactInputSingle<'a> {
    pub key: &'a V4PoolKey,
    pub input_asset: Address,
    pub output_asset: Address,
    pub wallet: Address,
    pub amount_in: U256,
    pub minimum: U256,
    pub deadline: u64,
    pub has_hop_price: bool,
    pub native_in: bool,
}

pub fn exact_input_single(request: ExactInputSingle<'_>) -> Result<(Bytes, U256)> {
    let ExactInputSingle {
        key,
        input_asset,
        output_asset,
        wallet,
        amount_in,
        minimum,
        deadline,
        has_hop_price,
        native_in,
    } = request;
    let pool = pool_key(key)?;
    let input = if native_in {
        Address::ZERO
    } else {
        input_asset
    };
    let zero_for_one = if input == pool.currency0 {
        true
    } else if input == pool.currency1 {
        false
    } else {
        return Err(error(
            "V4_POOL_MISMATCH",
            "validation",
            "Swap assets do not match the configured v4 pool currencies.",
        ));
    };
    let output = if zero_for_one {
        pool.currency1
    } else {
        pool.currency0
    };
    if output != output_asset
        || wallet == Address::ZERO
        || amount_in.is_zero()
        || minimum.is_zero()
        || deadline == 0
    {
        return Err(error(
            "V4_POOL_MISMATCH",
            "validation",
            "Swap direction, wallet, amount, output or deadline is invalid.",
        ));
    }
    let amount_in: u128 = amount_in.try_into().map_err(|_| {
        error(
            "V4_AMOUNT_TOO_LARGE",
            "validation",
            "Uniswap v4 single-pool exact input is limited to uint128.",
        )
    })?;
    let minimum: u128 = minimum.try_into().map_err(|_| {
        error(
            "V4_AMOUNT_TOO_LARGE",
            "validation",
            "Uniswap v4 single-pool minimum output is limited to uint128.",
        )
    })?;
    let swap = if has_hop_price {
        let scaled = U256::from(minimum) * U256::from(10u64).pow(U256::from(36));
        let price = scaled / U256::from(amount_in);
        UniversalRouter::encodeV4SingleCall {
            poolKey: pool,
            zeroForOne: zero_for_one,
            amountIn: amount_in,
            amountOutMinimum: minimum,
            minHopPriceX36: price,
            hookData: Bytes::new(),
        }
        .abi_encode()[4..]
            .to_vec()
    } else {
        UniversalRouter::encodeV4SingleLegacyCall {
            poolKey: pool,
            zeroForOne: zero_for_one,
            amountIn: amount_in,
            amountOutMinimum: minimum,
            hookData: Bytes::new(),
        }
        .abi_encode()[4..]
            .to_vec()
    };
    let settlement = UniversalRouter::encodeSettleAllCall {
        currency: input,
        maximumAmount: U256::from(amount_in),
    }
    .abi_encode()[4..]
        .to_vec();
    let take = UniversalRouter::encodeTakeAllCall {
        currency: output,
        minimumAmount: U256::from(minimum),
    }
    .abi_encode()[4..]
        .to_vec();
    let actions = Bytes::from(vec![SWAP_EXACT_IN_SINGLE, SETTLE_ALL, TAKE_ALL]);
    let params = vec![
        Bytes::from(swap),
        Bytes::from(settlement),
        Bytes::from(take),
    ];
    let inner = UniversalRouter::encodeV4SwapCall { actions, params }.abi_encode()[4..].to_vec();
    let calldata = UniversalRouter::executeCall {
        commands: Bytes::from(vec![V4_SWAP]),
        inputs: vec![Bytes::from(inner)],
        deadline: U256::from(deadline),
    }
    .abi_encode()
    .into();
    let _ = wallet; // V4 TAKE_ALL sends to the original router caller.
    Ok((
        calldata,
        if native_in {
            U256::from(amount_in)
        } else {
            U256::ZERO
        },
    ))
}

pub fn pool_id(key: &V4PoolKey) -> Result<String> {
    let pool = pool_key(key)?;
    let encoded = UniversalRouter::encodeV4SingleLegacyCall {
        poolKey: pool,
        zeroForOne: true,
        amountIn: 1,
        amountOutMinimum: 1,
        hookData: Bytes::new(),
    }
    .abi_encode();
    let hash = keccak256(&encoded[4..4 + 160]);
    Ok(format!("{hash:#x}"))
}
