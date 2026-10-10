//! Reviewed protocol contracts; asset identities are supplied by callers.
//! Source: https://github.com/Uniswap/contracts/tree/main/deployments
//! Version-specific encodings require the owned AMM fork verification.
use fin_ethereum::{RamsesConfig, RouterKind, UniswapConfig, V4Config};

pub fn reviewed_ramses(chain_id: u64) -> Option<RamsesConfig> {
    (chain_id == 4663).then(|| RamsesConfig {
        factory: "0xE0c4ceb92d08CA985bB70fe0a22fEb121A9854A8".into(),
        quoter: "0x4730e03EB4a58A5e20244062D5f9A99bCf5770a6".into(),
        router: "0xFCBBe2Af83F94e7E2a9C35a535B3A04719aFD2Ae".into(),
        pool_deployer: "0x4b37359BF291AbE8453692DB58d515a8b013Dca9".into(),
    })
}

pub fn reviewed_uniswap(chain_id: u64) -> Option<UniswapConfig> {
    let cfg = match chain_id {
        1 => UniswapConfig {
            factory: "0x1f98431c8ad98523631ae4a59f267346ea31f984".into(),
            quoter: "0x61ffe014ba17989e743c5f6cb21bf9697530b21e".into(),
            router: "0x68b3465833fb72a70ecdf485e0e4c7bd8665fc45".into(),
            router_kind: RouterKind::SwapRouter02,
            v4: Some(V4Config {
                router: "0x23617e59a5925b2a4bf75d73ff6711cd0b29de85".into(),
                quoter: "0x52f0e24d1c21c8a0cb1e5a5dd6198556bd9e1203".into(),
                pool_manager: "0x000000000004444c5dc75cb358380d2e3de08a90".into(),
                state_view: "0x7ffe42c4a5deea5b0fec41c94c136cf115597227".into(),
                router_has_hop_price: true,
            }),
        },
        42161 => UniswapConfig {
            factory: "0x1f98431c8ad98523631ae4a59f267346ea31f984".into(),
            quoter: "0x61ffe014ba17989e743c5f6cb21bf9697530b21e".into(),
            router: "0x68b3465833fb72a70ecdf485e0e4c7bd8665fc45".into(),
            router_kind: RouterKind::SwapRouter02,
            v4: Some(V4Config {
                router: "0x2d01411773c8c24805306e89a41f7855c3c4fe65".into(),
                quoter: "0x3972c00f7ed4885e145823eb7c655375d275a1c5".into(),
                pool_manager: "0x360e68faccca8ca495c1b759fd9eee466db9fb32".into(),
                state_view: "0x76fd297e2d437cd7f76d50f01afe6160f86e9990".into(),
                router_has_hop_price: true,
            }),
        },
        4663 => UniswapConfig {
            factory: "0x1f7d7550b1b028f7571e69a784071f0205fd2efa".into(),
            quoter: "0x33e885ed0ec9bf04ecfb19341582aadcb4c8a9e7".into(),
            router: "0xcaf681a66d020601342297493863e78c959e5cb2".into(),
            router_kind: RouterKind::SwapRouter02,
            v4: Some(V4Config {
                router: "0x204faca1764b154221e35c0d20abb3c525710498".into(),
                quoter: "0x8dc178efb8111bb0973dd9d722ebeff267c98f94".into(),
                pool_manager: "0x8366a39cc670b4001a1121b8f6a443a643e40951".into(),
                state_view: "0xf3334192d15450cdd385c8b70e03f9a6bd9e673b".into(),
                router_has_hop_price: true,
            }),
        },
        _ => return None,
    };
    Some(cfg)
}
