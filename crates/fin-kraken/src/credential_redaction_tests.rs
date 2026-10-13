//! The useful private permission read must never disclose the signing key.
use crate::{
    KrakenAccount, KrakenEndpoint as E, KrakenProduct, KrakenProfile, KrakenResponse,
    KrakenTransport,
};
use async_trait::async_trait;
use fin_domain::{ResourceId, Result};
use fin_exchange::{ExchangeProfile, kraken::DispatchGuard};
use serde_json::{Value, json};
use std::sync::Arc;
struct AuthFixture;
#[async_trait]
impl KrakenTransport for AuthFixture {
    async fn request(
        &self,
        e: E,
        a: Option<&KrakenAccount>,
        p: Value,
        g: Option<DispatchGuard>,
    ) -> Result<KrakenResponse> {
        assert_eq!(e, E::KrakenfuturesAuthGetApiKeysV3Check);
        assert_eq!(a.unwrap().credential_ref, "synthetic-reference-only");
        assert_eq!(p, json!({}));
        assert!(g.is_none());
        Ok(KrakenResponse::Json(
            json!({"apiKey":"SYNTHETIC-NON-LIVE-PUBLIC-KEY","accountUid":"123e4567-e89b-42d3-a456-426614174000","iiban":"SYNTHETIC","permissions":{"general":"READ_ONLY","transfer":"NO_ACCESS"},"nested":{"API-Key":"SYNTHETIC-NON-LIVE-PUBLIC-KEY","apiSecret":"SYNTHETIC-NON-LIVE-SECRET"}}),
        ))
    }
}
#[tokio::test]
async fn signed_permission_read_redacts_camelcase_key_but_preserves_native_identity_and_permissions()
 {
    let a = KrakenAccount {
        name: "derivatives".into(),
        resource: ResourceId("krakenfutures:synthetic-only".into()),
        credential_ref: "synthetic-reference-only".into(),
        physical_identity: "123e4567-e89b-42d3-a456-426614174000".into(),
        identity_verified: true,
        product: KrakenProduct::Derivatives,
        wallet_account_id: None,
        default_wallet_account_id: None,
    };
    let p = KrakenProfile::new(KrakenProduct::Derivatives, vec![a], Arc::new(AuthFixture)).unwrap();
    let read = p
        .read(
            E::KrakenfuturesAuthGetApiKeysV3Check.operation(),
            &json!({"account":"derivatives"}),
        )
        .await
        .unwrap();
    assert!(read["data"].get("apiKey").is_none());
    assert_eq!(
        read["data"]["accountUid"],
        "123e4567-e89b-42d3-a456-426614174000"
    );
    assert_eq!(read["data"]["permissions"]["general"], "READ_ONLY");
    assert!(!read.to_string().contains("SYNTHETIC-NON-LIVE-PUBLIC-KEY"));
    assert!(!read.to_string().contains("SYNTHETIC-NON-LIVE-SECRET"));
}

struct MissingConfigurationPermission;
struct ExpiryFixture(Value);
#[async_trait]
impl KrakenTransport for ExpiryFixture {
    async fn request(
        &self,
        e: E,
        _: Option<&KrakenAccount>,
        p: Value,
        g: Option<DispatchGuard>,
    ) -> Result<KrakenResponse> {
        assert_eq!(e, E::KrakenPrivatePostGetApiKeyInfo);
        assert_eq!(p, json!({}));
        assert!(g.is_none());
        Ok(KrakenResponse::Json(
            json!({"result":{"permissions":["query-funds"],"valid_until":self.0}}),
        ))
    }
}
#[tokio::test]
async fn malformed_or_expired_native_key_metadata_cannot_silently_skip_expiry_validation() {
    let a = KrakenAccount {
        name: "spot".into(),
        resource: ResourceId("kraken:synthetic-only".into()),
        credential_ref: "synthetic-reference-only".into(),
        physical_identity: "SYNTHETIC".into(),
        identity_verified: true,
        product: KrakenProduct::Spot,
        wallet_account_id: None,
        default_wallet_account_id: None,
    };
    for (expiry, code) in [
        (json!("1"), "KRAKEN_KEY_EXPIRED"),
        (json!(1), "KRAKEN_KEY_EXPIRY_UNKNOWN"),
        (Value::Null, "KRAKEN_KEY_EXPIRY_UNKNOWN"),
        (json!({"invalid":true}), "KRAKEN_KEY_EXPIRY_UNKNOWN"),
    ] {
        let p = KrakenProfile::new(
            KrakenProduct::Spot,
            vec![a.clone()],
            Arc::new(ExpiryFixture(expiry)),
        )
        .unwrap();
        assert_eq!(
            p.spot_permissions(&a, &["query-funds"])
                .await
                .unwrap_err()
                .code,
            code
        );
    }
    let p = KrakenProfile::new(
        KrakenProduct::Spot,
        vec![a.clone()],
        Arc::new(ExpiryFixture(json!("0"))),
    )
    .unwrap();
    p.spot_permissions(&a, &["query-funds"]).await.unwrap();
}
#[async_trait]
impl KrakenTransport for MissingConfigurationPermission {
    async fn request(
        &self,
        e: E,
        _: Option<&KrakenAccount>,
        p: Value,
        g: Option<DispatchGuard>,
    ) -> Result<KrakenResponse> {
        assert_eq!(e, E::KrakenPrivatePostGetApiKeyInfo);
        assert_eq!(p, json!({}));
        assert!(g.is_none());
        Ok(KrakenResponse::Json(
            json!({"result":{"permissions":["query-funds"],"valid_until":"0"}}),
        ))
    }
}
#[tokio::test]
async fn administrative_missing_permissions_fail_before_any_mutation() {
    let a = KrakenAccount {
        name: "spot".into(),
        resource: ResourceId("kraken:synthetic-only".into()),
        credential_ref: "synthetic-only".into(),
        physical_identity: "SYNTHETIC".into(),
        identity_verified: true,
        product: KrakenProduct::Spot,
        wallet_account_id: None,
        default_wallet_account_id: None,
    };
    let p = KrakenProfile::new(
        KrakenProduct::Spot,
        vec![a],
        Arc::new(MissingConfigurationPermission),
    )
    .unwrap();
    let error = p
        .preflight_configuration(
            E::KrakenPrivatePostAddExport.operation(),
            &json!({"account":"spot","report":"trades","description":"synthetic report"}),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, "KRAKEN_PERMISSION_DENIED");
    assert!(error.message.contains("export-data"));
}
