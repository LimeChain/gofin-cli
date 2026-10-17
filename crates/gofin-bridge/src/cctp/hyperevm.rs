//! Safe HyperEVM-to-HyperCore native-USDC deposit calldata.
use super::*;

pub const CHAIN_ID: u64 = 999;
pub const CORE_DEPOSIT_WALLET: &str = "0x6B9E773128f453f5c2C60935Ee2DE2CBc5390A24";
pub const NATIVE_USDC: &str = "0xb88339CB7199b77E23DB6E890353E22632Ba630f";
pub const SPOT_DEX: u32 = u32::MAX;

sol! {
    interface CoreDepositWallet {
        function depositFor(address recipient, uint256 amount, uint32 destinationDex) external;
    }
}

/// Build the required ERC-20 approval. The allowance is finite and exactly
/// equal to the approved deposit amount.
pub fn approve_call(amount: &Quantity) -> Result<EvmCall> {
    ensure(
        !amount.is_zero(),
        "CCTP_VALIDATION_FAILED",
        "bridge",
        "Deposit amount must be positive.",
    )?;
    Ok(EvmCall {
        chain_id: CHAIN_ID,
        to: NATIVE_USDC.into(),
        value: Quantity::from_u64(0),
        data: hex_bytes(
            &Token::approveCall {
                spender: address(CORE_DEPOSIT_WALLET)?,
                amount: u256(amount)?,
            }
            .abi_encode(),
        ),
    })
}

/// Build CoreDepositWallet.depositFor(proxy, amount, uint32::MAX). Sending
/// native USDC directly to the proxy does not credit HyperCore.
pub fn deposit_call(proxy: Address, amount: &Quantity) -> Result<EvmCall> {
    ensure(
        proxy != Address::ZERO && !amount.is_zero(),
        "CCTP_VALIDATION_FAILED",
        "bridge",
        "HyperCore proxy and deposit amount must be nonzero.",
    )?;
    Ok(EvmCall {
        chain_id: CHAIN_ID,
        to: CORE_DEPOSIT_WALLET.into(),
        value: Quantity::from_u64(0),
        data: hex_bytes(
            &CoreDepositWallet::depositForCall {
                recipient: proxy,
                amount: u256(amount)?,
                destinationDex: SPOT_DEX,
            }
            .abi_encode(),
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approve_and_deposit_are_finite_and_spot_scoped() {
        let proxy: Address = "0x1234567890123456789012345678901234567890"
            .parse()
            .unwrap();
        let amount = Quantity::from_u64(1_000_000);
        let approve = approve_call(&amount).unwrap();
        let deposit = deposit_call(proxy, &amount).unwrap();
        assert_eq!(approve.chain_id, 999);
        assert_eq!(approve.to, NATIVE_USDC);
        assert_eq!(deposit.to, CORE_DEPOSIT_WALLET);
        assert!(deposit.data.starts_with("0x"));
        assert!(deposit_call(Address::ZERO, &amount).is_err());
        assert!(approve_call(&Quantity::from_u64(0)).is_err());
    }
}
