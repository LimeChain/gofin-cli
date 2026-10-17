use gofin_binance::{BinanceEndpoint as E, BinanceRequest as R};

#[test]
fn settings_endpoint_boundary_rejects_unreviewed_settings_and_accepts_exact_native_symbols() {
    for symbol in ["ETHUSDT", "币安人生USDT", "SYNTHETIC_USDT"] {
        R::new(E::FuturesLeverage)
            .push("symbol", symbol)
            .push("leverage", "3")
            .validate()
            .unwrap();
        R::new(E::FuturesMarginType)
            .push("symbol", symbol)
            .push("marginType", "ISOLATED")
            .validate()
            .unwrap();
    }
    for leverage in ["0", "126", "1.5", "-1"] {
        assert!(
            R::new(E::FuturesLeverage)
                .push("symbol", "ETHUSDT")
                .push("leverage", leverage)
                .validate()
                .is_err()
        );
    }
    assert!(
        R::new(E::FuturesMarginType)
            .push("symbol", "ETHUSDT")
            .push("marginType", "HEDGE")
            .validate()
            .is_err()
    );
    assert!(
        R::new(E::FuturesLeverage)
            .push("symbol", "ETHUSDT&leverage=100")
            .push("leverage", "3")
            .validate()
            .is_err()
    );
    assert!(
        R::new(E::FuturesLeverage)
            .push("symbol", "ETHUSDT")
            .push("leverage", "3")
            .push("timestamp", "1")
            .validate()
            .is_err()
    );
}

#[test]
fn same_account_futures_margin_transfer_rejects_other_assets_and_cross_account_routes() {
    let request = |from, to| {
        R::new(E::UniversalTransfer)
            .push("asset", "USDT")
            .push("amount", "10")
            .push("clientTranId", "gofin-synthetic-transfer")
            .push("fromAccountType", from)
            .push("toAccountType", to)
    };
    for (from, to) in [("SPOT", "USDT_FUTURE"), ("USDT_FUTURE", "SPOT")] {
        request(from, to).validate().unwrap();
        request(from, to)
            .push("fromEmail", "sub@example.invalid")
            .push("toEmail", "sub@example.invalid")
            .validate()
            .unwrap();
        assert!(
            request(from, to)
                .push("toEmail", "sub@example.invalid")
                .validate()
                .is_err()
        );
        assert!(
            request(from, to)
                .push("fromEmail", "first@example.invalid")
                .push("toEmail", "second@example.invalid")
                .validate()
                .is_err()
        );
        let mut other = request(from, to);
        other.params[0].value = "BTC".into();
        assert!(other.validate().is_err());
    }
    assert!(request("USDT_FUTURE", "USDT_FUTURE").validate().is_err());
}
