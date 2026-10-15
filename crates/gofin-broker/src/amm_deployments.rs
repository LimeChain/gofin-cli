//! Reviewed protocol contracts; asset identities are supplied by callers.
//! Source: https://github.com/Uniswap/contracts/tree/main/deployments
//! Version-specific encodings require the owned AMM fork verification.
use gofin_ethereum::{
    RamsesConfig, RouterKind, UniswapConfig, V3DeploymentConfig, V3Dex, V3PoolAbi,
    V3RouterMetadataAbi, V4Config,
};
use std::collections::BTreeMap;

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
        // https://developers.uniswap.org/docs/protocols/v3/deployments/v3-base-deployments
        // https://developers.uniswap.org/docs/protocols/v4/deployments
        8453 => UniswapConfig {
            factory: "0x33128a8fc17869897dce68ed026d694621f6fdfd".into(),
            quoter: "0x3d4e44eb1374240ce5f1b871ab261cd16335b76a".into(),
            router: "0x2626664c2603336e57b271c5c0b26f421741e481".into(),
            router_kind: RouterKind::SwapRouter02,
            v4: Some(V4Config {
                router: "0xd6145b2d3f379919e8cdeda7b97e37c4b2ca9c40".into(),
                quoter: "0x0d5e0f971ed27fbff6c2837bf31316121532048d".into(),
                pool_manager: "0x498581ff718922c3f8e6a244956af099b2652b2b".into(),
                state_view: "0xa3c0c9b65bad0b08107aa264b0f3db444b867a71".into(),
                router_has_hop_price: true,
            }),
        },
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

// Sushi pinned official v3-core/v3-periphery deployment JSON;
// Pancake: https://developer.pancakeswap.finance/contracts/v3/addresses
// Exact pool/event and router ABIs are profile properties; no asset constants.
pub fn reviewed_sushi(chain_id: u64) -> Option<V3DeploymentConfig> {
    let cfg = match chain_id {
        1 => V3DeploymentConfig {
            factory: "0xbACEB8eC6b9355Dfc0269C18bac9d6E2Bdc29C4F".into(),
            quoter: "0x64e8802FE490fa7cc61d3463958199161Bb608A7".into(),
            router: "0x2E6cd2d30aa43f40aa81619ff4b6E0a41479B13F".into(),
            router_kind: RouterKind::LegacyV3,
            pool_deployer: None,
            pool_abi: V3PoolAbi::UniswapV3,
            router_metadata_abi: V3RouterMetadataAbi::UniswapV3,
            allowed_fee_tiers: vec![100, 500, 3000, 10000],
        },
        42161 => V3DeploymentConfig {
            factory: "0x1af415a1EbA07a4986a52B6f2e7dE7003D82231e".into(),
            quoter: "0x0524E833cCD057e4d7A296e3aaAb9f7675964Ce1".into(),
            router: "0x8A21F6768C1f8075791D08546Dadf6daA0bE820c".into(),
            router_kind: RouterKind::LegacyV3,
            pool_deployer: None,
            pool_abi: V3PoolAbi::UniswapV3,
            router_metadata_abi: V3RouterMetadataAbi::UniswapV3,
            allowed_fee_tiers: vec![100, 500, 3000, 10000],
        },
        _ => return None,
    };
    Some(cfg)
}
pub fn reviewed_pancakeswap(chain_id: u64) -> Option<V3DeploymentConfig> {
    let cfg = match chain_id {
        1 => V3DeploymentConfig {
            factory: "0x0BFbCF9fa4f9C56B0F40a671Ad40E0805A091865".into(),
            quoter: "0xB048Bbc1Ee6b733FFfCFb9e9CeF7375518e25997".into(),
            router: "0x1b81D678ffb9C0263b24A97847620C99d213eB14".into(),
            router_kind: RouterKind::LegacyV3,
            pool_deployer: Some("0x41ff9AA7e16B8B1a8a8dc4f0eFacd93D02d071c9".into()),
            pool_abi: V3PoolAbi::PancakeV3,
            router_metadata_abi: V3RouterMetadataAbi::PancakeV3,
            allowed_fee_tiers: vec![100, 500, 2500, 10000],
        },
        42161 => V3DeploymentConfig {
            factory: "0x0BFbCF9fa4f9C56B0F40a671Ad40E0805A091865".into(),
            quoter: "0xB048Bbc1Ee6b733FFfCFb9e9CeF7375518e25997".into(),
            router: "0x1b81D678ffb9C0263b24A97847620C99d213eB14".into(),
            router_kind: RouterKind::LegacyV3,
            pool_deployer: Some("0x41ff9AA7e16B8B1a8a8dc4f0eFacd93D02d071c9".into()),
            pool_abi: V3PoolAbi::PancakeV3,
            router_metadata_abi: V3RouterMetadataAbi::PancakeV3,
            allowed_fee_tiers: vec![100, 500, 2500, 10000],
        },
        4663 => V3DeploymentConfig {
            factory: "0x0BFbCF9fa4f9C56B0F40a671Ad40E0805A091865".into(),
            quoter: "0x8553AA1615549A86882151784b329B017aA7c832".into(),
            router: "0x13f4EA83D0bd40E75C8222255bc855a974568Dd4".into(),
            router_kind: RouterKind::SwapRouter02,
            pool_deployer: Some("0x41ff9AA7e16B8B1a8a8dc4f0eFacd93D02d071c9".into()),
            pool_abi: V3PoolAbi::PancakeV3,
            router_metadata_abi: V3RouterMetadataAbi::PancakeV3,
            allowed_fee_tiers: vec![100, 500, 2500, 10000],
        },
        _ => return None,
    };
    Some(cfg)
}
pub fn reviewed_v3_deployments(chain_id: u64) -> BTreeMap<V3Dex, V3DeploymentConfig> {
    [
        (V3Dex::Sushi, reviewed_sushi(chain_id)),
        (V3Dex::PancakeSwap, reviewed_pancakeswap(chain_id)),
    ]
    .into_iter()
    .filter_map(|(dex, config)| config.map(|config| (dex, config)))
    .collect()
}
