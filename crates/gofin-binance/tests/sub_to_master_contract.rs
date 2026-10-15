use gofin_binance::{BinanceEndpoint, BinanceError, ExactDecimal, SubToMasterTransferIntent};

#[test]
fn sub_to_master_request_keeps_exact_amount_and_excludes_arbitrary_routes() {
    let mut request = SubToMasterTransferIntent {
        asset: "ETH".into(),
        amount: ExactDecimal::parse("0.12345678901234567890123456789").unwrap(),
    }
    .request()
    .unwrap();
    assert_eq!(request.endpoint, BinanceEndpoint::SubToMasterTransfer);
    assert_eq!(
        request.endpoint.method_path(),
        ("POST", "/sapi/v1/sub-account/transfer/subToMaster")
    );
    assert_eq!(
        request.get("amount"),
        Some("0.12345678901234567890123456789")
    );
    for key in [
        "clientTranId",
        "fromEmail",
        "toEmail",
        "toAccountType",
        "fromAccountType",
    ] {
        let arbitrary = request.clone().push(key, "SYNTHETIC");
        assert_eq!(
            arbitrary.validate(),
            Err(BinanceError::UnsupportedParameter)
        );
    }
    request = request.push("amount", "1");
    assert_eq!(request.validate(), Err(BinanceError::DuplicateParameter));
}

#[test]
fn sub_to_master_request_rejects_zero_and_invalid_asset_identifiers() {
    assert_eq!(
        SubToMasterTransferIntent {
            asset: "ETH".into(),
            amount: ExactDecimal::zero()
        }
        .request(),
        Err(BinanceError::InvalidRequest)
    );
    for asset in ["", " ETH", "ETH\n", "\t"] {
        assert_eq!(
            SubToMasterTransferIntent {
                asset: asset.into(),
                amount: ExactDecimal::parse("1").unwrap()
            }
            .request(),
            Err(BinanceError::InvalidRequest)
        );
    }
}
