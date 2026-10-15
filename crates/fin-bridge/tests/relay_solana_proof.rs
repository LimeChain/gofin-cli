use fin_bridge::relay::solana::derive_order_address;

#[test]
fn relay_sdk_order_address_vector() {
    // relay-settlement/packages/sdk/test/withdrawal-execution.test.ts
    assert_eq!(
        derive_order_address(
            "ethereum",
            "0x9400f1b21cb527d7fa3d3eabba93557a18ebe7a2ca4e471cfe5e4c5b4ca7f767",
        )
        .unwrap(),
        "0xf85a144b09258a2059b514360936b633e941b685"
    );
}
