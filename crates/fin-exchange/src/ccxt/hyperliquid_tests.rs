use super::*;
use ccxt::exchange_generated::ExchangeBase;
use k256::ecdsa::{RecoveryId, Signature, VerifyingKey};
use rmpv::Value as MpValue;
use sha3::{Digest, Keccak256};
use std::{
    io::{Read, Write},
    net::TcpListener,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
use tokio::sync::oneshot;

struct Fixture {
    url: String,
    received: oneshot::Receiver<String>,
    count: Arc<AtomicUsize>,
    worker: thread::JoinHandle<()>,
}

impl Fixture {
    fn new() -> Self {
        Self::new_with_body(r#"{"status":"ok","response":{"type":"default"}}"#.into())
    }

    fn new_with_body(response_body: String) -> Self {
        Self::new_with_status(200, response_body)
    }

    fn new_with_status(status: u16, response_body: String) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let count = Arc::new(AtomicUsize::new(0));
        let accepted = count.clone();
        let (sender, received) = oneshot::channel();
        let worker = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(20);
            while Instant::now() < deadline {
                let Ok((mut stream, _)) = listener.accept() else {
                    thread::sleep(Duration::from_millis(5));
                    continue;
                };
                accepted.fetch_add(1, Ordering::SeqCst);
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(15)))
                    .unwrap();
                let mut bytes = Vec::new();
                let mut chunk = [0u8; 4096];
                let mut complete = false;
                let mut diagnostic = None;
                loop {
                    match stream.read(&mut chunk) {
                        Ok(0) => {
                            diagnostic = Some("client closed before a complete request".to_owned());
                            break;
                        }
                        Ok(read) => {
                            bytes.extend_from_slice(&chunk[..read]);
                            match request_is_complete(&bytes) {
                                Ok(true) => {
                                    complete = true;
                                    break;
                                }
                                Ok(false) => {}
                                Err(error) => {
                                    diagnostic = Some(error);
                                    break;
                                }
                            }
                        }
                        Err(error) => {
                            diagnostic = Some(format!("request read failed: {error}"));
                            break;
                        }
                    }
                }
                let request = String::from_utf8_lossy(&bytes).into_owned();
                if !complete {
                    let diagnostic = diagnostic.unwrap_or_else(|| {
                        "request ended before Content-Length bytes arrived".to_owned()
                    });
                    let captured = format!("{request}\n[fixture incomplete: {diagnostic}]");
                    let _ = sender.send(captured);
                    let body = serde_json::json!({
                        "fixture_error": diagnostic,
                        "synthetic_request": request,
                    })
                    .to_string();
                    let _ = write!(
                        stream,
                        "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    return;
                }
                let _ = sender.send(request);
                let body = response_body;
                let reason = if status == 200 {
                    "OK"
                } else if status == 500 {
                    "Internal Server Error"
                } else {
                    "Synthetic Error"
                };
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                return;
            }
        });
        Self {
            url,
            received,
            count,
            worker,
        }
    }

    async fn request(self) -> (String, usize) {
        let request = tokio::time::timeout(Duration::from_secs(18), self.received)
            .await
            .unwrap()
            .unwrap();
        self.worker.join().unwrap();
        assert!(
            request_is_complete(request.as_bytes()).unwrap_or(false),
            "fixture did not receive a complete synthetic request: {request:?}"
        );
        (request, self.count.load(Ordering::SeqCst))
    }
}

fn test_client(url: &str) -> CcxtClient {
    CcxtClient::for_test_with_timeout(url, Duration::from_secs(16)).unwrap()
}

fn synthetic_signer() -> HyperliquidSigner {
    HyperliquidSigner {
        private_key: Zeroizing::new(format!("0x{:064x}", 1)),
        wallet_address: "0x7E5F4552091A69125d5DfCb7b8C2659029395Bdf".into(),
    }
}

fn body(request: &str) -> &str {
    request.split_once("\r\n\r\n").unwrap().1
}

fn request_is_complete(bytes: &[u8]) -> Result<bool, String> {
    let Some(header_end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") else {
        return Ok(false);
    };
    let headers = std::str::from_utf8(&bytes[..header_end])
        .map_err(|_| "request headers are not valid UTF-8/ASCII".to_owned())?;
    let lengths: Vec<_> = headers
        .lines()
        .skip(1)
        .filter_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then_some(value.trim())
        })
        .collect();
    if lengths.len() != 1 {
        return Err(format!(
            "expected exactly one Content-Length header, found {}",
            lengths.len()
        ));
    }
    let content_length = lengths[0]
        .parse::<usize>()
        .map_err(|_| format!("invalid Content-Length header: {:?}", lengths[0]))?;
    let body_start = header_end + 4;
    Ok(bytes.len().saturating_sub(body_start) >= content_length)
}

#[test]
fn info_queries_are_fixed_ordered_and_owner_is_explicit() {
    // Internal enum names describe observations; these are the actual native
    // endpoint types, which do not have a `State` suffix.
    for (request, native_type) in [
        (
            HyperliquidInfoRequest::UserAbstractionState {
                owner: "0x1111111111111111111111111111111111111111".into(),
            },
            "userAbstraction",
        ),
        (
            HyperliquidInfoRequest::UserDexAbstractionState {
                owner: "0x1111111111111111111111111111111111111111".into(),
            },
            "userDexAbstraction",
        ),
    ] {
        assert_eq!(
            ccxt_value_to_json(hl_info_value(&request).unwrap()).unwrap(),
            serde_json::json!({"type":native_type,"user":"0x1111111111111111111111111111111111111111"})
        );
    }
    let value = hl_info_value(&HyperliquidInfoRequest::UserFillsByTime {
        owner: "0x1111111111111111111111111111111111111111".into(),
        start_time_ms: 10,
        end_time_ms: 20,
    })
    .unwrap();
    let ccxt::Value::Dict(value) = value else {
        panic!("expected map")
    };
    assert_eq!(
        value.keys().map(String::as_str).collect::<Vec<_>>(),
        ["type", "user", "startTime", "endTime"]
    );
    assert_eq!(value["type"].as_str(), Some("userFillsByTime"));
    let twap = hl_info_value(&HyperliquidInfoRequest::UserTwapSliceFills {
        owner: "0x1111111111111111111111111111111111111111".into(),
    })
    .unwrap();
    let ccxt::Value::Dict(twap) = twap else {
        panic!("expected map")
    };
    assert_eq!(
        twap.keys().map(String::as_str).collect::<Vec<_>>(),
        ["type", "user"]
    );
    assert_eq!(twap["type"].as_str(), Some("userTwapSliceFills"));
    let active = ccxt_value_to_json(
        hl_info_value(&HyperliquidInfoRequest::ActiveAssetData {
            owner: "0x1111111111111111111111111111111111111111".into(),
            coin: "ETH".into(),
        })
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        active,
        serde_json::json!({"type":"activeAssetData","user":"0x1111111111111111111111111111111111111111","coin":"ETH"})
    );
    assert!(
        hl_info_value(&HyperliquidInfoRequest::ActiveAssetData {
            owner: "not-an-owner".into(),
            coin: "ETH".into()
        })
        .is_err()
    );
    let time =
        ccxt_value_to_json(hl_info_value(&HyperliquidInfoRequest::ServerTime).unwrap()).unwrap();
    assert_eq!(time, serde_json::json!({"type":"exchangeStatus"}));
    assert_eq!(
        value["user"].as_str(),
        Some("0x1111111111111111111111111111111111111111")
    );
    let referral = ccxt_value_to_json(
        hl_info_value(&HyperliquidInfoRequest::ReferralState {
            owner: "0x1111111111111111111111111111111111111111".into(),
        })
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        referral,
        serde_json::json!({"type":"referral","user":"0x1111111111111111111111111111111111111111"})
    );
    let builder_fee = ccxt_value_to_json(
        hl_info_value(&HyperliquidInfoRequest::MaxBuilderFee {
            owner: "0x1111111111111111111111111111111111111111".into(),
            builder: "0x2222222222222222222222222222222222222222".into(),
        })
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        builder_fee,
        serde_json::json!({"type":"maxBuilderFee","user":"0x1111111111111111111111111111111111111111","builder":"0x2222222222222222222222222222222222222222"})
    );
    let vault = ccxt_value_to_json(
        hl_info_value(&HyperliquidInfoRequest::VaultDetails {
            vault_address: "0x3333333333333333333333333333333333333333".into(),
            user: Some("0x1111111111111111111111111111111111111111".into()),
        })
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        vault,
        serde_json::json!({"type":"vaultDetails","vaultAddress":"0x3333333333333333333333333333333333333333","user":"0x1111111111111111111111111111111111111111"})
    );
    assert!(
        hl_info_value(&HyperliquidInfoRequest::VaultDetails {
            vault_address: "bad".into(),
            user: None,
        })
        .is_err()
    );
    assert!(
        hl_info_value(&HyperliquidInfoRequest::OpenOrders {
            owner: "wallet".into()
        })
        .is_err()
    );
    let hip3_open_orders = ccxt_value_to_json(
        hl_info_value(&HyperliquidInfoRequest::FrontendOpenOrdersForDex {
            owner: "0x1111111111111111111111111111111111111111".into(),
            dex: "xyz".into(),
        })
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        hip3_open_orders,
        serde_json::json!({"type":"frontendOpenOrders","user":"0x1111111111111111111111111111111111111111","dex":"xyz"})
    );
    assert!(
        hl_info_value(&HyperliquidInfoRequest::FrontendOpenOrdersForDex {
            owner: "0x1111111111111111111111111111111111111111".into(),
            dex: "  xyz".into(),
        })
        .is_err()
    );
    assert!(
        hl_info_value(&HyperliquidInfoRequest::CandleSnapshot {
            coin: "BTC".into(),
            interval: "3h".into(),
            start_time_ms: 1,
            end_time_ms: 2
        })
        .is_err()
    );
    let dex = ccxt_value_to_json(
        hl_info_value(&HyperliquidInfoRequest::MetaAndAssetCtxsForDex { dex: "xyz".into() })
            .unwrap(),
    )
    .unwrap();
    assert_eq!(dex["type"], "metaAndAssetCtxs");
    assert_eq!(dex["dex"], "xyz");
    let dex_state = ccxt_value_to_json(
        hl_info_value(&HyperliquidInfoRequest::ClearinghouseStateForDex {
            owner: "0x1111111111111111111111111111111111111111".into(),
            dex: "xyz".into(),
        })
        .unwrap(),
    )
    .unwrap();
    assert_eq!(dex_state["type"], "clearinghouseState");
    assert_eq!(dex_state["dex"], "xyz");
    let cloid = ccxt_value_to_json(
        hl_info_value(&HyperliquidInfoRequest::OrderStatusByClientId {
            owner: "0x1111111111111111111111111111111111111111".into(),
            client_order_id: "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
        })
        .unwrap(),
    )
    .unwrap();
    assert_eq!(cloid["type"], "orderStatus");
    assert_eq!(cloid["oid"], "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    assert!(
        hl_info_value(&HyperliquidInfoRequest::OrderStatusByClientId {
            owner: "0x1111111111111111111111111111111111111111".into(),
            client_order_id: "not-a-cloid".into(),
        })
        .is_err()
    );
}

#[test]
fn explorer_requests_are_typed_separate_and_require_exact_identity() {
    let hash = format!("0x{}", "a".repeat(64));
    let value = ccxt_value_to_json(
        hl_explorer_value(&HyperliquidExplorerRequest::TxDetails { hash: hash.clone() }).unwrap(),
    )
    .unwrap();
    assert_eq!(value, serde_json::json!({"type":"txDetails","hash":hash}));
    let user = "0x1111111111111111111111111111111111111111";
    assert_eq!(
        ccxt_value_to_json(
            hl_explorer_value(&HyperliquidExplorerRequest::UserDetails { user: user.into() })
                .unwrap()
        )
        .unwrap(),
        serde_json::json!({"type":"userDetails","user":user})
    );
    assert!(
        hl_explorer_value(&HyperliquidExplorerRequest::TxDetails {
            hash: "0x12".into()
        })
        .is_err()
    );
    assert!(
        hl_explorer_value(&HyperliquidExplorerRequest::UserDetails {
            user: "wallet".into()
        })
        .is_err()
    );
}

#[test]
fn action_enum_rejects_unknown_fields_and_exposes_exact_unsigned_wire_binding() {
    let order = HyperliquidOrder {
        asset: 0,
        is_buy: true,
        price: "123.45".into(),
        size: "0.0123".into(),
        reduce_only: false,
        order_type: HyperliquidOrderType::Trigger {
            trigger_price: "123.00".into(),
            is_market: true,
            tpsl: HyperliquidTpsl::StopLoss,
        },
        client_order_id: None,
    };
    let envelope = HyperliquidActionEnvelope {
        nonce_ms: 1_700_000_000_000,
        vault_address: Some("0x1111111111111111111111111111111111111111".into()),
        action: HyperliquidAction::Order {
            orders: vec![order],
            grouping: HyperliquidOrderGrouping::Na,
            builder_fee: None,
        },
    };
    let binding = envelope.action_wire_json().unwrap();
    assert_eq!(binding["nonce"], 1_700_000_000_000u64);
    assert_eq!(
        binding["vaultAddress"],
        "1111111111111111111111111111111111111111"
    );
    assert_eq!(binding["action"]["type"], "order");
    assert_eq!(binding["action"]["orders"][0]["t"]["trigger"]["tpsl"], "sl");
    assert!(
        serde_json::from_str::<HyperliquidActionEnvelope>(
            r#"{"nonce_ms":1,"action":{"kind":"reserve_request_weight","weight":1,"raw":"x"}}"#
        )
        .is_err()
    );
    let noop = HyperliquidActionEnvelope {
        nonce_ms: 17,
        vault_address: None,
        action: HyperliquidAction::Noop {},
    };
    let encoded = serde_json::to_value(noop).unwrap();
    assert_eq!(encoded["action"], serde_json::json!({"kind":"noop"}));
    assert!(
        serde_json::from_str::<HyperliquidActionEnvelope>(
            r#"{"nonce_ms":17,"action":{"kind":"noop","nonce_ms":99}}"#
        )
        .is_err()
    );
}

#[test]
fn every_supported_native_and_user_signed_action_has_a_fixed_wire_fixture() {
    let user = "0x1111111111111111111111111111111111111111".to_owned();
    let order = HyperliquidOrder {
        asset: 0,
        is_buy: true,
        price: "123.45".into(),
        size: "0.0123".into(),
        reduce_only: false,
        order_type: HyperliquidOrderType::Limit {
            time_in_force: HyperliquidTimeInForce::Gtc,
        },
        client_order_id: Some("0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into()),
    };
    let cases = vec![
        (HyperliquidAction::Noop {}, "noop"),
        (
            HyperliquidAction::Order {
                orders: vec![order.clone()],
                grouping: HyperliquidOrderGrouping::Na,
                builder_fee: None,
            },
            "order",
        ),
        (
            HyperliquidAction::Order {
                orders: vec![order.clone()],
                grouping: HyperliquidOrderGrouping::Na,
                builder_fee: Some(HyperliquidBuilderFee {
                    address: user.clone(),
                    fee_tenths_bps: 75,
                }),
            },
            "order",
        ),
        (
            HyperliquidAction::Cancel {
                cancels: vec![HyperliquidCancel {
                    asset: 0,
                    order_id: 1,
                }],
            },
            "cancel",
        ),
        (
            HyperliquidAction::Amend {
                modifies: vec![HyperliquidAmend {
                    asset: 0,
                    order_id: 1,
                    order,
                }],
            },
            "batchModify",
        ),
        (
            HyperliquidAction::UpdateLeverage {
                asset: 0,
                leverage: 3,
                is_cross: true,
            },
            "updateLeverage",
        ),
        (
            HyperliquidAction::UpdateIsolatedMargin {
                asset: 0,
                amount_usdc: "2.5".into(),
            },
            "updateIsolatedMargin",
        ),
        (
            HyperliquidAction::TwapOrder {
                asset: 0,
                is_buy: true,
                size: "1".into(),
                reduce_only: false,
                duration_minutes: 5,
                randomize: true,
            },
            "twapOrder",
        ),
        (
            HyperliquidAction::TwapCancel {
                asset: 0,
                twap_id: 1,
            },
            "twapCancel",
        ),
        (
            HyperliquidAction::ScheduledCancel {
                time_ms: Some(1_700_000_000_000),
            },
            "scheduledCancel",
        ),
        (
            HyperliquidAction::VaultTransfer {
                vault_address: user.clone(),
                is_deposit: true,
                amount_usd_micro: 1_000_000,
            },
            "vaultTransfer",
        ),
        (
            HyperliquidAction::CreateVault {
                name: "synthetic".into(),
                description: "fixture".into(),
                initial_usd_micro: 1_000_000,
            },
            "createVault",
        ),
        (
            HyperliquidAction::SubAccountTransfer {
                sub_account_user: user.clone(),
                is_deposit: true,
                usd_micro: 1_000_000,
            },
            "subAccountTransfer",
        ),
        (
            HyperliquidAction::SubAccountSpotTransfer {
                sub_account_user: user.clone(),
                is_deposit: false,
                token: "USDC".into(),
                amount: "1.25".into(),
            },
            "subAccountSpotTransfer",
        ),
        (
            HyperliquidAction::CreateSubAccount {
                name: "synthetic".into(),
                expires_after_ms: Some(1_700_000_000_000),
            },
            "createSubAccount",
        ),
        (
            HyperliquidAction::ReserveRequestWeight {
                weight: 1,
                destination: None,
            },
            "reserveRequestWeight",
        ),
        (
            HyperliquidAction::SetRef {
                code: "synthetic".into(),
            },
            "setReferrer",
        ),
        (
            HyperliquidAction::UserAbstraction {
                user: user.clone(),
                abstraction: HyperliquidAbstraction::Disabled,
                agent: true,
            },
            "agentSetAbstraction",
        ),
        (
            HyperliquidAction::UserAbstraction {
                user: user.clone(),
                abstraction: HyperliquidAbstraction::UnifiedAccount,
                agent: false,
            },
            "userSetAbstraction",
        ),
        (
            HyperliquidAction::UserDexAbstraction {
                user: user.clone(),
                enabled: true,
                agent: true,
            },
            "agentEnableDexAbstraction",
        ),
        (
            HyperliquidAction::UserDexAbstraction {
                user: user.clone(),
                enabled: false,
                agent: false,
            },
            "userDexAbstraction",
        ),
        (
            HyperliquidAction::UsdSend {
                destination: user.clone(),
                amount_usdc: "1.25".into(),
                time_ms: 1_700_000_000_000,
            },
            "usdSend",
        ),
        (
            HyperliquidAction::Withdraw {
                destination: user.clone(),
                amount_usdc: "1.25".into(),
                time_ms: 1_700_000_000_000,
            },
            "withdraw3",
        ),
        (
            HyperliquidAction::UsdClassTransfer {
                amount_usdc: "1.25".into(),
                to_perp: true,
                nonce: 1_700_000_000_000,
            },
            "usdClassTransfer",
        ),
        (
            HyperliquidAction::ApproveBuilderFee {
                builder: user.clone(),
                max_fee_rate: "0.01%".into(),
                nonce: 1_700_000_000_000,
            },
            "approveBuilderFee",
        ),
    ];
    for (action, expected_type) in cases {
        let agent_eip = matches!(
            action,
            HyperliquidAction::UserAbstraction { agent: false, .. }
                | HyperliquidAction::UserDexAbstraction { agent: false, .. }
        );
        let user_signed = matches!(
            action,
            HyperliquidAction::UsdSend { .. }
                | HyperliquidAction::Withdraw { .. }
                | HyperliquidAction::UsdClassTransfer { .. }
                | HyperliquidAction::ApproveBuilderFee { .. }
                | HyperliquidAction::UserAbstraction { agent: false, .. }
                | HyperliquidAction::UserDexAbstraction { agent: false, .. }
        );
        let envelope = HyperliquidActionEnvelope {
            nonce_ms: 1_700_000_000_000,
            vault_address: if user_signed || agent_eip {
                None
            } else {
                Some(user.clone())
            },
            action,
        };
        let wire = envelope.action_wire_json().unwrap();
        assert_eq!(wire["action"]["type"], expected_type, "{expected_type}");
        assert_eq!(wire["nonce"], 1_700_000_000_000u64);
        if expected_type == "noop" {
            assert_eq!(wire["action"], serde_json::json!({"type":"noop"}));
        }
    }
    assert!(
        hl_action_value(
            &HyperliquidAction::UsdClassTransfer {
                amount_usdc: "-1".into(),
                to_perp: true,
                nonce: 1_700_000_000_000,
            },
            1_700_000_000_000,
        )
        .is_err()
    );
}

#[test]
fn reserve_request_weight_only_serializes_an_explicit_valid_destination() {
    let destination = "0x2222222222222222222222222222222222222222".to_owned();
    let with_destination = HyperliquidActionEnvelope {
        nonce_ms: 17,
        vault_address: None,
        action: HyperliquidAction::ReserveRequestWeight {
            weight: 12,
            destination: Some(destination.clone()),
        },
    }
    .action_wire_json()
    .unwrap();
    assert_eq!(
        with_destination["action"],
        serde_json::json!({
            "type":"reserveRequestWeight",
            "weight":12,
            "destination":destination,
        })
    );

    let without_destination = HyperliquidActionEnvelope {
        nonce_ms: 17,
        vault_address: None,
        action: HyperliquidAction::ReserveRequestWeight {
            weight: 12,
            destination: None,
        },
    }
    .action_wire_json()
    .unwrap();
    assert_eq!(without_destination["action"]["weight"], 12);
    assert!(without_destination["action"].get("destination").is_none());

    assert!(
        hl_action_value(
            &HyperliquidAction::ReserveRequestWeight {
                weight: 12,
                destination: Some("not-an-address".into()),
            },
            17,
        )
        .is_err()
    );
}

#[test]
fn order_builder_fee_is_optional_typed_ordered_and_native_integer_tenths_bps() {
    let order = HyperliquidOrder {
        asset: 0,
        is_buy: true,
        price: "10".into(),
        size: "1".into(),
        reduce_only: false,
        order_type: HyperliquidOrderType::Limit {
            time_in_force: HyperliquidTimeInForce::Gtc,
        },
        client_order_id: None,
    };
    let action = |builder_fee| HyperliquidAction::Order {
        orders: vec![order.clone()],
        grouping: HyperliquidOrderGrouping::Na,
        builder_fee,
    };
    let plain = HyperliquidActionEnvelope {
        nonce_ms: 1_700_000_000_000,
        vault_address: None,
        action: action(None),
    }
    .action_wire_json()
    .unwrap();
    assert!(plain["action"].get("builder").is_none());

    let builder = HyperliquidBuilderFee {
        address: "0x2222222222222222222222222222222222222222".into(),
        fee_tenths_bps: 75,
    };
    let with_builder = HyperliquidActionEnvelope {
        nonce_ms: 1_700_000_000_000,
        vault_address: None,
        action: action(Some(builder.clone())),
    }
    .action_wire_json()
    .unwrap();
    assert_eq!(
        with_builder["action"]["builder"],
        serde_json::json!({"b":builder.address,"f":75})
    );

    let native = hl_action_value(&action(Some(builder.clone())), 1_700_000_000_000).unwrap();
    let action_fields = native.as_map().unwrap();
    assert_eq!(
        action_fields.keys().map(String::as_str).collect::<Vec<_>>(),
        ["type", "orders", "grouping", "builder"]
    );
    let builder_fields = action_fields["builder"].as_map().unwrap();
    assert_eq!(
        builder_fields
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["b", "f"]
    );
    assert_eq!(builder_fields["f"].as_i64(), Some(75));

    for invalid in [
        HyperliquidBuilderFee {
            address: "not-an-address".into(),
            fee_tenths_bps: 75,
        },
        HyperliquidBuilderFee {
            address: builder.address.clone(),
            fee_tenths_bps: 0,
        },
        HyperliquidBuilderFee {
            address: builder.address.clone(),
            fee_tenths_bps: 1001,
        },
    ] {
        assert!(hl_action_value(&action(Some(invalid)), 1).is_err());
    }
}

#[tokio::test]
async fn info_dispatch_uses_only_public_info_endpoint_and_explicit_user() {
    let server = Fixture::new();
    let client = test_client(&server.url);
    let result = client
        .hyperliquid_info(HyperliquidInfoRequest::OpenOrders {
            owner: "0x1111111111111111111111111111111111111111".into(),
        })
        .await;
    let (request, count) = server.request().await;
    assert!(
        result.is_ok(),
        "CCXT info request failed: {result:?}; synthetic request: {request:?}"
    );
    result.unwrap();
    assert_eq!(count, 1);
    assert!(request.starts_with("POST /info "));
    let wire: serde_json::Value = serde_json::from_str(body(&request)).unwrap();
    assert_eq!(
        wire,
        serde_json::json!({"type":"openOrders","user":"0x1111111111111111111111111111111111111111"})
    );
}

#[tokio::test]
async fn typed_info_preserves_json_primitives_and_exact_number_lexemes() {
    let owner = "0x1111111111111111111111111111111111111111";
    let cases = [
        (
            HyperliquidInfoRequest::UserAbstractionState {
                owner: owner.into(),
            },
            r#""default""#,
            serde_json::Value::String("default".into()),
        ),
        (
            HyperliquidInfoRequest::UserDexAbstractionState {
                owner: owner.into(),
            },
            "null",
            serde_json::Value::Null,
        ),
        (
            HyperliquidInfoRequest::UserDexAbstractionState {
                owner: owner.into(),
            },
            "false",
            serde_json::Value::Bool(false),
        ),
        (
            HyperliquidInfoRequest::MaxBuilderFee {
                owner: owner.into(),
                builder: "0x2222222222222222222222222222222222222222".into(),
            },
            "9007199254740993",
            serde_json::from_str("9007199254740993").unwrap(),
        ),
    ];

    for (request, response_body, expected) in cases {
        let server = Fixture::new_with_body(response_body.into());
        let client = test_client(&server.url);
        let result = client.hyperliquid_info(request).await;
        let (synthetic_request, count) = server.request().await;
        assert!(
            result.is_ok(),
            "typed primitive response was rejected: {result:?}; request: {synthetic_request:?}"
        );
        assert_eq!(count, 1);
        assert_eq!(result.unwrap(), expected);
    }
}

#[tokio::test]
async fn primitive_response_allowlist_rejects_wrong_shapes() {
    let owner = "0x1111111111111111111111111111111111111111";
    let cases = [
        (
            HyperliquidInfoRequest::UserAbstractionState {
                owner: owner.into(),
            },
            "{}",
        ),
        (
            HyperliquidInfoRequest::UserDexAbstractionState {
                owner: owner.into(),
            },
            r#""true""#,
        ),
        (
            HyperliquidInfoRequest::MaxBuilderFee {
                owner: owner.into(),
                builder: "0x2222222222222222222222222222222222222222".into(),
            },
            "1.5",
        ),
        (
            HyperliquidInfoRequest::OpenOrders {
                owner: owner.into(),
            },
            "null",
        ),
    ];

    for (request, response_body) in cases {
        let server = Fixture::new_with_body(response_body.into());
        let client = test_client(&server.url);
        let result = client.hyperliquid_info(request).await;
        let (synthetic_request, count) = server.request().await;
        let error = result.expect_err("unsupported primitive response shape must fail closed");
        assert_eq!(error.kind, CcxtErrorKind::InvalidResponse);
        assert_eq!(error.http_status, Some(200));
        assert_eq!(count, 1, "synthetic request: {synthetic_request:?}");
    }
}

#[tokio::test]
async fn explorer_dispatch_uses_only_typed_public_explorer_endpoint() {
    let server = Fixture::new();
    let client = test_client(&server.url);
    let result = client
        .hyperliquid_explorer(HyperliquidExplorerRequest::TxDetails {
            hash: format!("0x{}", "a".repeat(64)),
        })
        .await;
    let (request, count) = server.request().await;
    assert!(
        result.is_ok(),
        "CCXT Explorer request failed: {result:?}; synthetic request: {request:?}"
    );
    result.unwrap();
    assert_eq!(count, 1);
    assert!(
        request.starts_with("POST /explorer "),
        "unexpected synthetic request: {request:?}"
    );
    let wire: serde_json::Value = serde_json::from_str(body(&request)).unwrap();
    assert_eq!(wire["type"], "txDetails");
    assert_eq!(wire["hash"], format!("0x{}", "a".repeat(64)));
}

#[tokio::test]
async fn signed_action_guard_blocks_dispatch_and_allowed_action_uses_exchange() {
    let blocked = Fixture::new();
    let client = test_client(&blocked.url);
    let action = HyperliquidActionEnvelope {
        nonce_ms: 1_700_000_000_000,
        vault_address: Some("0x2222222222222222222222222222222222222222".into()),
        action: HyperliquidAction::SetRef {
            code: "synthetic-test-only".into(),
        },
    };
    let denied = client
        .hyperliquid_action(action.clone(), synthetic_signer(), Arc::new(|| false))
        .await;
    assert_eq!(denied.unwrap_err().kind, CcxtErrorKind::PermissionDenied);
    assert_eq!(blocked.count.load(Ordering::SeqCst), 0);

    let allowed = Fixture::new();
    let client = test_client(&allowed.url);
    let result = client
        .hyperliquid_action(action, synthetic_signer(), Arc::new(|| true))
        .await;
    let (request, count) = allowed.request().await;
    assert!(
        result.is_ok(),
        "CCXT action request failed: {result:?}; synthetic request: {request:?}"
    );
    result.unwrap();
    assert_eq!(count, 1);
    assert!(request.starts_with("POST /exchange "), "{request}");
    let wire: serde_json::Value = serde_json::from_str(body(&request)).unwrap();
    assert_eq!(wire["action"]["type"], "setReferrer");
    assert_eq!(wire["action"]["code"], "synthetic-test-only");
    assert_eq!(wire["nonce"], 1_700_000_000_000u64);
    assert_eq!(
        wire["vaultAddress"],
        "2222222222222222222222222222222222222222"
    );
    assert!(wire["signature"]["r"].as_str().unwrap().starts_with("0x"));
    assert!(wire["signature"]["s"].as_str().unwrap().starts_with("0x"));
}

fn synthetic_set_ref_envelope() -> HyperliquidActionEnvelope {
    HyperliquidActionEnvelope {
        nonce_ms: 1_700_000_000_000,
        vault_address: None,
        action: HyperliquidAction::SetRef {
            code: "synthetic-test-only".into(),
        },
    }
}

#[test]
fn native_action_rejection_shape_requires_error_status_and_nonblank_text() {
    assert!(is_native_hyperliquid_action_rejection(&serde_json::json!({
        "status": "err",
        "response": "synthetic rejection"
    })));
    assert!(is_native_hyperliquid_action_rejection(&serde_json::json!({
        "status": "err",
        "response": "synthetic rejection",
        "futureNativeField": true
    })));
    for malformed in [
        serde_json::json!({"status": false, "response": "synthetic rejection"}),
        serde_json::json!({"status": "ok", "response": "synthetic rejection"}),
        serde_json::json!({"status": "err", "response": false}),
        serde_json::json!({"status": "err", "response": "  \n "}),
        serde_json::json!({"status": "err"}),
        serde_json::json!("synthetic rejection"),
    ] {
        assert!(!is_native_hyperliquid_action_rejection(&malformed));
    }
}

#[tokio::test]
async fn typed_action_preserves_exact_native_http_200_rejection_once() {
    let native = serde_json::json!({
        "status": "err",
        "response": "Insufficient margin for order."
    });
    let server = Fixture::new_with_body(native.to_string());
    let client = test_client(&server.url);
    let result = client
        .hyperliquid_action(
            synthetic_set_ref_envelope(),
            synthetic_signer(),
            Arc::new(|| true),
        )
        .await;
    let (request, count) = server.request().await;
    assert_eq!(
        result.expect("validated native rejection is durable response evidence"),
        native
    );
    assert_eq!(count, 1, "typed action must never replay after a rejection");
    assert!(request.starts_with("POST /exchange "), "{request}");
}

#[tokio::test]
async fn native_rejection_receipt_conversion_is_limited_to_valid_http_200_actions() {
    let server = Fixture::new_with_status(
        500,
        r#"{"status":"err","response":"synthetic HTTP failure"}"#.into(),
    );
    let client = test_client(&server.url);
    let result = client
        .hyperliquid_action(
            synthetic_set_ref_envelope(),
            synthetic_signer(),
            Arc::new(|| true),
        )
        .await;
    let (_, count) = server.request().await;
    let error = result.expect_err("HTTP 500 must remain a transport failure");
    assert_eq!(error.http_status, Some(500));
    assert_eq!(error.kind, CcxtErrorKind::Rejected);
    assert_eq!(count, 1);

    let server = Fixture::new_with_body(r#"{"status":"err","response":" "}"#.into());
    let client = test_client(&server.url);
    let malformed = client
        .hyperliquid_action(
            synthetic_set_ref_envelope(),
            synthetic_signer(),
            Arc::new(|| true),
        )
        .await;
    let (_, count) = server.request().await;
    assert!(
        malformed.is_err(),
        "empty native error text is not a receipt"
    );
    assert_eq!(count, 1);

    let server =
        Fixture::new_with_body(r#"{"status":"err","response":"synthetic info failure"}"#.into());
    let client = test_client(&server.url);
    let info = client
        .hyperliquid_info(HyperliquidInfoRequest::ExchangeStatus)
        .await;
    let (_, count) = server.request().await;
    assert!(
        info.is_err(),
        "non-action CCXT error handling stays unchanged"
    );
    assert_eq!(count, 1);

    let server = Fixture::new_with_body("not-json".into());
    let client = test_client(&server.url);
    let invalid_json = client
        .hyperliquid_action(
            synthetic_set_ref_envelope(),
            synthetic_signer(),
            Arc::new(|| true),
        )
        .await;
    let (_, count) = server.request().await;
    assert!(
        invalid_json.is_err(),
        "invalid JSON is not a rejection receipt"
    );
    assert_eq!(count, 1);
}

#[tokio::test]
async fn generic_method_api_cannot_bypass_typed_hyperliquid_transport() {
    let client = test_client("http://127.0.0.1:1");
    let credentials = || Credentials {
        api_key: Zeroizing::new("synthetic-key".into()),
        secret: Zeroizing::new("synthetic-secret".into()),
    };
    let info = client
        .request(
            ExchangeId::Hyperliquid,
            Method::HyperliquidTypedInfo,
            Some(credentials()),
            vec![("type".into(), "userFills".into())],
        )
        .await
        .unwrap_err();
    assert_eq!(info.kind, CcxtErrorKind::InvalidRequest);
    let action = client
        .request_guarded(
            ExchangeId::Hyperliquid,
            Method::HyperliquidTypedAction,
            Some(credentials()),
            vec![("action".into(), "{}".into())],
            Arc::new(|| true),
        )
        .await
        .unwrap_err();
    assert_eq!(action.kind, CcxtErrorKind::InvalidRequest);
}

fn keccak(bytes: &[u8]) -> [u8; 32] {
    Keccak256::digest(bytes).into()
}

fn word_u64(value: u64) -> [u8; 32] {
    let mut word = [0u8; 32];
    word[24..].copy_from_slice(&value.to_be_bytes());
    word
}

#[derive(Clone, Copy)]
enum EipValue<'a> {
    Text(&'a str),
    Address(&'a str),
    Bool(bool),
    U64(u64),
}

fn independent_user_digest(primary_type: &str, fields: &[(&str, &str, EipValue<'_>)]) -> [u8; 32] {
    let type_definition = format!(
        "{}({})",
        primary_type,
        fields
            .iter()
            .map(|(name, kind, _)| format!("{kind} {name}"))
            .collect::<Vec<_>>()
            .join(",")
    );
    let mut structure = keccak(type_definition.as_bytes()).to_vec();
    for (_, kind, value) in fields {
        match (kind, value) {
            (&"string", EipValue::Text(value)) => {
                structure.extend_from_slice(&keccak(value.as_bytes()))
            }
            (&"address", EipValue::Address(value)) => {
                let bytes = hex::decode(value.trim_start_matches("0x")).unwrap();
                let mut word = [0u8; 32];
                word[32 - bytes.len()..].copy_from_slice(&bytes);
                structure.extend_from_slice(&word);
            }
            (&"bool", EipValue::Bool(value)) => {
                let mut word = [0u8; 32];
                word[31] = u8::from(*value);
                structure.extend_from_slice(&word);
            }
            (&"uint64", EipValue::U64(value)) => structure.extend_from_slice(&word_u64(*value)),
            _ => panic!("mismatched synthetic EIP-712 fixture type"),
        }
    }
    let struct_hash = keccak(&structure);
    let domain_type_hash = keccak(
        b"EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)",
    );
    let mut domain = Vec::new();
    domain.extend_from_slice(&domain_type_hash);
    domain.extend_from_slice(&keccak(b"HyperliquidSignTransaction"));
    domain.extend_from_slice(&keccak(b"1"));
    domain.extend_from_slice(&word_u64(421614));
    domain.extend_from_slice(&[0u8; 32]);
    let domain_hash = keccak(&domain);
    let mut digest = vec![0x19, 0x01];
    digest.extend_from_slice(&domain_hash);
    digest.extend_from_slice(&struct_hash);
    keccak(&digest)
}

fn assert_signature_recovers_test_key(signature: &ccxt::Value, digest: &[u8; 32]) {
    let signature = signature.as_map().unwrap();
    let r: [u8; 32] = hex::decode(signature["r"].as_str().unwrap().trim_start_matches("0x"))
        .unwrap()
        .try_into()
        .unwrap();
    let s: [u8; 32] = hex::decode(signature["s"].as_str().unwrap().trim_start_matches("0x"))
        .unwrap()
        .try_into()
        .unwrap();
    let sig = Signature::from_scalars(r, s).unwrap();
    let recovery = RecoveryId::try_from((signature["v"].as_i64().unwrap() - 27) as u8).unwrap();
    let recovered = VerifyingKey::recover_from_prehash(digest, &sig, recovery).unwrap();
    let public = recovered.to_encoded_point(false);
    let address_hash = keccak(&public.as_bytes()[1..]);
    assert_eq!(
        hex::encode(&address_hash[12..]),
        "7e5f4552091a69125d5dfcb7b8c2659029395bdf"
    );
}

#[tokio::test]
async fn ccxt_user_signed_builders_match_independent_eip712_vectors() {
    let user = "0x1111111111111111111111111111111111111111";
    let builder = "0x2222222222222222222222222222222222222222";
    let nonce = 1_700_000_000_000u64;
    let mut core = HyperliquidCore::new(Some(ccxt::Value::from(
        serde_json::json!({"options":{"sandboxMode":false}}),
    )));
    core.privateKey = ccxt::Value::str(format!("0x{:064x}", 1));
    let cases = vec![
        (
            "build_usd_send_sig",
            hl_object([
                ("hyperliquidChain", hl_str("Mainnet")),
                ("destination", hl_str(user)),
                ("amount", hl_str("1.25")),
                ("time", ccxt::Value::Int(nonce as i64)),
            ]),
            "HyperliquidTransaction:UsdSend",
            vec![
                ("hyperliquidChain", "string", EipValue::Text("Mainnet")),
                ("destination", "string", EipValue::Text(user)),
                ("amount", "string", EipValue::Text("1.25")),
                ("time", "uint64", EipValue::U64(nonce)),
            ],
        ),
        (
            "build_withdraw_sig",
            hl_object([
                ("hyperliquidChain", hl_str("Mainnet")),
                ("destination", hl_str(user)),
                ("amount", hl_str("1.25")),
                ("time", ccxt::Value::Int(nonce as i64)),
            ]),
            "HyperliquidTransaction:Withdraw",
            vec![
                ("hyperliquidChain", "string", EipValue::Text("Mainnet")),
                ("destination", "string", EipValue::Text(user)),
                ("amount", "string", EipValue::Text("1.25")),
                ("time", "uint64", EipValue::U64(nonce)),
            ],
        ),
        (
            "build_usd_class_send_sig",
            hl_object([
                ("hyperliquidChain", hl_str("Mainnet")),
                ("amount", hl_str("1.25")),
                ("toPerp", ccxt::Value::Bool(true)),
                ("nonce", ccxt::Value::Int(nonce as i64)),
            ]),
            "HyperliquidTransaction:UsdClassTransfer",
            vec![
                ("hyperliquidChain", "string", EipValue::Text("Mainnet")),
                ("amount", "string", EipValue::Text("1.25")),
                ("toPerp", "bool", EipValue::Bool(true)),
                ("nonce", "uint64", EipValue::U64(nonce)),
            ],
        ),
        (
            "build_user_abstraction_sig",
            hl_object([
                ("hyperliquidChain", hl_str("Mainnet")),
                ("user", hl_str(user)),
                ("abstraction", hl_str("unifiedAccount")),
                ("nonce", ccxt::Value::Int(nonce as i64)),
            ]),
            "HyperliquidTransaction:UserSetAbstraction",
            vec![
                ("hyperliquidChain", "string", EipValue::Text("Mainnet")),
                ("user", "address", EipValue::Address(user)),
                ("abstraction", "string", EipValue::Text("unifiedAccount")),
                ("nonce", "uint64", EipValue::U64(nonce)),
            ],
        ),
        (
            "build_user_dex_abstraction_sig",
            hl_object([
                ("hyperliquidChain", hl_str("Mainnet")),
                ("user", hl_str(user)),
                ("enabled", ccxt::Value::Bool(true)),
                ("nonce", ccxt::Value::Int(nonce as i64)),
            ]),
            "HyperliquidTransaction:UserDexAbstraction",
            vec![
                ("hyperliquidChain", "string", EipValue::Text("Mainnet")),
                ("user", "address", EipValue::Address(user)),
                ("enabled", "bool", EipValue::Bool(true)),
                ("nonce", "uint64", EipValue::U64(nonce)),
            ],
        ),
        (
            "build_approve_builder_fee_sig",
            hl_object([
                ("hyperliquidChain", hl_str("Mainnet")),
                ("maxFeeRate", hl_str("0.01%")),
                ("builder", hl_str(builder)),
                ("nonce", ccxt::Value::Int(nonce as i64)),
            ]),
            "HyperliquidTransaction:ApproveBuilderFee",
            vec![
                ("hyperliquidChain", "string", EipValue::Text("Mainnet")),
                ("maxFeeRate", "string", EipValue::Text("0.01%")),
                ("builder", "address", EipValue::Address(builder)),
                ("nonce", "uint64", EipValue::U64(nonce)),
            ],
        ),
    ];
    for (builder, payload, primary_type, fields) in cases {
        let signature = ccxt::runtime::call_typed(core.call_dynamic(builder, vec![payload]))
            .await
            .unwrap();
        assert_signature_recovers_test_key(
            &signature,
            &independent_user_digest(primary_type, &fields),
        );
    }
}

#[tokio::test]
async fn ccxt_l1_signature_matches_independent_messagepack_eip712_verification() {
    let key = format!("0x{:064x}", 1);
    let mut core = HyperliquidCore::new(Some(ccxt::Value::from(
        serde_json::json!({"options":{"sandboxMode":false}}),
    )));
    core.privateKey = ccxt::Value::str(key);
    let mut action_fields = indexmap::IndexMap::new();
    action_fields.insert("type".to_owned(), ccxt::Value::str("setReferrer"));
    action_fields.insert("code".to_owned(), ccxt::Value::str("synthetic-test-only"));
    let action = ccxt::Value::Map(action_fields);
    let nonce = 1_700_000_000_000u64;
    let signed = ccxt::runtime::call_typed(core.call_dynamic(
        "sign_l1_action",
        vec![action, ccxt::Value::Int(nonce as i64), ccxt::Value::Null],
    ))
    .await
    .unwrap();
    let signature = signed.as_map().unwrap();
    let r: [u8; 32] = hex::decode(signature["r"].as_str().unwrap().trim_start_matches("0x"))
        .unwrap()
        .try_into()
        .unwrap();
    let s: [u8; 32] = hex::decode(signature["s"].as_str().unwrap().trim_start_matches("0x"))
        .unwrap()
        .try_into()
        .unwrap();
    let sig = Signature::from_scalars(r, s).unwrap();
    let v = signature["v"].as_i64().unwrap();
    let recovery = RecoveryId::try_from((v - 27) as u8).unwrap();

    let action_msgpack = MpValue::Map(vec![
        (
            MpValue::String("type".into()),
            MpValue::String("setReferrer".into()),
        ),
        (
            MpValue::String("code".into()),
            MpValue::String("synthetic-test-only".into()),
        ),
    ]);
    let mut bytes = Vec::new();
    rmpv::encode::write_value(&mut bytes, &action_msgpack).unwrap();
    bytes.extend_from_slice(&nonce.to_be_bytes());
    bytes.push(0); // no vault address
    let connection_id = keccak(&bytes);

    let mut domain_type =
        b"EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)"
            .to_vec();
    let domain_type_hash = keccak(&domain_type);
    domain_type.clear();
    let mut domain = Vec::new();
    domain.extend_from_slice(&domain_type_hash);
    domain.extend_from_slice(&keccak(b"Exchange"));
    domain.extend_from_slice(&keccak(b"1"));
    domain.extend_from_slice(&word_u64(1337));
    domain.extend_from_slice(&[0u8; 32]);
    let domain_hash = keccak(&domain);

    let agent_type_hash = keccak(b"Agent(string source,bytes32 connectionId)");
    let mut agent = Vec::new();
    agent.extend_from_slice(&agent_type_hash);
    agent.extend_from_slice(&keccak(b"a")); // mainnet source byte
    agent.extend_from_slice(&connection_id);
    let agent_hash = keccak(&agent);
    let mut digest_input = vec![0x19, 0x01];
    digest_input.extend_from_slice(&domain_hash);
    digest_input.extend_from_slice(&agent_hash);
    let digest = keccak(&digest_input);
    let recovered = VerifyingKey::recover_from_prehash(&digest, &sig, recovery).unwrap();
    let public = recovered.to_encoded_point(false);
    let address_hash = keccak(&public.as_bytes()[1..]);
    assert_eq!(
        hex::encode(&address_hash[12..]),
        "7e5f4552091a69125d5dfcb7b8c2659029395bdf"
    );
}

#[tokio::test]
async fn noop_uses_exact_envelope_nonce_and_independent_l1_signature_vector() {
    let nonce = 1_700_000_000_123u64;
    let envelope = HyperliquidActionEnvelope {
        nonce_ms: nonce,
        vault_address: None,
        action: HyperliquidAction::Noop {},
    };
    let binding = envelope.action_wire_json().unwrap();
    assert_eq!(binding["action"], serde_json::json!({"type":"noop"}));
    assert_eq!(binding["nonce"], nonce);
    assert_eq!(binding.as_object().unwrap().len(), 2);

    let server = Fixture::new_with_body(r#"{"status":"ok","response":{"type":"default"}}"#.into());
    let client = test_client(&server.url);
    let response = client
        .hyperliquid_action(envelope, synthetic_signer(), Arc::new(|| true))
        .await
        .unwrap();
    let (request, count) = server.request().await;
    assert_eq!(count, 1);
    assert!(request.starts_with("POST /exchange "), "{request}");
    assert_eq!(response["status"], "ok");
    assert_eq!(response["response"]["type"], "default");

    let wire: serde_json::Value = serde_json::from_str(body(&request)).unwrap();
    assert_eq!(wire["action"], serde_json::json!({"type":"noop"}));
    assert_eq!(wire["nonce"], nonce);
    assert_eq!(wire.as_object().unwrap().len(), 3);
    assert!(wire.get("vaultAddress").is_none());

    let mut action_fields = indexmap::IndexMap::new();
    action_fields.insert("type".to_owned(), ccxt::Value::str("noop"));
    let action = ccxt::Value::Map(action_fields);
    let mut core = HyperliquidCore::new(Some(ccxt::Value::from(
        serde_json::json!({"options":{"sandboxMode":false}}),
    )));
    core.privateKey = ccxt::Value::str(format!("0x{:064x}", 1));
    let signed = ccxt::runtime::call_typed(core.call_dynamic(
        "sign_l1_action",
        vec![action, ccxt::Value::Int(nonce as i64), ccxt::Value::Null],
    ))
    .await
    .unwrap();

    let action_msgpack = MpValue::Map(vec![(
        MpValue::String("type".into()),
        MpValue::String("noop".into()),
    )]);
    let mut bytes = Vec::new();
    rmpv::encode::write_value(&mut bytes, &action_msgpack).unwrap();
    bytes.extend_from_slice(&nonce.to_be_bytes());
    bytes.push(0); // Owner-scoped action; no vault or subaccount target.
    let connection_id = keccak(&bytes);
    let domain_type_hash = keccak(
        b"EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)",
    );
    let mut domain = Vec::new();
    domain.extend_from_slice(&domain_type_hash);
    domain.extend_from_slice(&keccak(b"Exchange"));
    domain.extend_from_slice(&keccak(b"1"));
    domain.extend_from_slice(&word_u64(1337));
    domain.extend_from_slice(&[0u8; 32]);
    let domain_hash = keccak(&domain);
    let mut agent = Vec::new();
    agent.extend_from_slice(&keccak(b"Agent(string source,bytes32 connectionId)"));
    agent.extend_from_slice(&keccak(b"a"));
    agent.extend_from_slice(&connection_id);
    let agent_hash = keccak(&agent);
    let mut digest_input = vec![0x19, 0x01];
    digest_input.extend_from_slice(&domain_hash);
    digest_input.extend_from_slice(&agent_hash);
    let digest = keccak(&digest_input);
    assert_signature_recovers_test_key(&signed, &digest);
    assert_signature_recovers_test_key(&ccxt::Value::from(wire["signature"].clone()), &digest);

    let rejection = serde_json::json!({
        "status": "err",
        "response": "nonce already used"
    });
    let server = Fixture::new_with_body(rejection.to_string());
    let client = test_client(&server.url);
    let response = client
        .hyperliquid_action(
            HyperliquidActionEnvelope {
                nonce_ms: nonce,
                vault_address: None,
                action: HyperliquidAction::Noop {},
            },
            synthetic_signer(),
            Arc::new(|| true),
        )
        .await
        .unwrap();
    let (request, count) = server.request().await;
    assert_eq!(response, rejection);
    assert_eq!(count, 1);
    assert!(request.starts_with("POST /exchange "), "{request}");
}

#[tokio::test]
async fn delegated_l1_target_is_bound_to_native_hash_but_keeps_owner_signer() {
    async fn sign_l1(
        core: &mut HyperliquidCore,
        action: ccxt::Value,
        nonce: u64,
        vault: Option<&str>,
    ) -> ccxt::Value {
        ccxt::runtime::call_typed(core.call_dynamic(
            "sign_l1_action",
            vec![
                action,
                ccxt::Value::Int(nonce as i64),
                hl_vault_address(vault),
                ccxt::Value::Null,
            ],
        ))
        .await
        .unwrap()
    }

    let nonce = 1_700_000_000_000u64;
    let target = "0x2222222222222222222222222222222222222222";
    let mut core = HyperliquidCore::new(Some(ccxt::Value::from(
        serde_json::json!({"options":{"sandboxMode":false}}),
    )));
    core.privateKey = ccxt::Value::str(format!("0x{:064x}", 1));
    let mut fields = indexmap::IndexMap::new();
    fields.insert("type".to_owned(), ccxt::Value::str("setReferrer"));
    fields.insert("code".to_owned(), ccxt::Value::str("synthetic-test-only"));
    let action = ccxt::Value::Map(fields);
    let action_mp = MpValue::Map(vec![
        (
            MpValue::String("type".into()),
            MpValue::String("setReferrer".into()),
        ),
        (
            MpValue::String("code".into()),
            MpValue::String("synthetic-test-only".into()),
        ),
    ]);

    let owner_signature = sign_l1(&mut core, action.clone(), nonce, None).await;
    let delegated_signature = sign_l1(&mut core, action, nonce, Some(target)).await;

    let independent_digest = |vault: Option<&str>| {
        let mut bytes = Vec::new();
        rmpv::encode::write_value(&mut bytes, &action_mp).unwrap();
        bytes.extend_from_slice(&nonce.to_be_bytes());
        match vault {
            None => bytes.push(0),
            Some(address) => {
                bytes.push(1);
                bytes.extend_from_slice(&hex::decode(address.strip_prefix("0x").unwrap()).unwrap());
            }
        }
        let connection_id = keccak(&bytes);
        let domain_type = keccak(
            b"EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)",
        );
        let mut domain = Vec::new();
        domain.extend_from_slice(&domain_type);
        domain.extend_from_slice(&keccak(b"Exchange"));
        domain.extend_from_slice(&keccak(b"1"));
        domain.extend_from_slice(&word_u64(1337));
        domain.extend_from_slice(&[0u8; 32]);
        let domain_hash = keccak(&domain);
        let agent_type_hash = keccak(b"Agent(string source,bytes32 connectionId)");
        let mut agent = Vec::new();
        agent.extend_from_slice(&agent_type_hash);
        agent.extend_from_slice(&keccak(b"a"));
        agent.extend_from_slice(&connection_id);
        let agent_hash = keccak(&agent);
        let mut digest_input = vec![0x19, 0x01];
        digest_input.extend_from_slice(&domain_hash);
        digest_input.extend_from_slice(&agent_hash);
        keccak(&digest_input)
    };

    let recover = |signature: &ccxt::Value, digest: &[u8; 32]| {
        let values = signature.as_map().unwrap();
        let r: [u8; 32] = hex::decode(values["r"].as_str().unwrap().trim_start_matches("0x"))
            .unwrap()
            .try_into()
            .unwrap();
        let s: [u8; 32] = hex::decode(values["s"].as_str().unwrap().trim_start_matches("0x"))
            .unwrap()
            .try_into()
            .unwrap();
        let sig = Signature::from_scalars(r, s).unwrap();
        let recovery = RecoveryId::try_from((values["v"].as_i64().unwrap() - 27) as u8).unwrap();
        let key = VerifyingKey::recover_from_prehash(digest, &sig, recovery).unwrap();
        let public = key.to_encoded_point(false);
        let address_hash = keccak(&public.as_bytes()[1..]);
        hex::encode(&address_hash[12..])
    };

    let owner_digest = independent_digest(None);
    let delegated_digest = independent_digest(Some(target));
    assert_ne!(
        owner_digest, delegated_digest,
        "target must change L1 connection hash"
    );
    assert_eq!(
        recover(&owner_signature, &owner_digest),
        "7e5f4552091a69125d5dfcb7b8c2659029395bdf"
    );
    assert_eq!(
        recover(&delegated_signature, &delegated_digest),
        "7e5f4552091a69125d5dfcb7b8c2659029395bdf",
        "delegated action must still be signed by the protected master key"
    );

    let envelope = HyperliquidActionEnvelope {
        nonce_ms: nonce,
        vault_address: Some(target.into()),
        action: HyperliquidAction::SetRef {
            code: "synthetic-test-only".into(),
        },
    };
    let wire = envelope.action_wire_json().unwrap();
    assert_eq!(wire["vaultAddress"], &target[2..]);

    for action in [
        HyperliquidAction::UsdSend {
            destination: target.into(),
            amount_usdc: "1".into(),
            time_ms: nonce,
        },
        HyperliquidAction::Withdraw {
            destination: target.into(),
            amount_usdc: "1".into(),
            time_ms: nonce,
        },
        HyperliquidAction::UsdClassTransfer {
            amount_usdc: "1".into(),
            to_perp: true,
            nonce,
        },
        HyperliquidAction::ApproveBuilderFee {
            builder: target.into(),
            max_fee_rate: "0.01%".into(),
            nonce,
        },
        HyperliquidAction::UserAbstraction {
            user: target.into(),
            abstraction: HyperliquidAbstraction::UnifiedAccount,
            agent: false,
        },
        HyperliquidAction::UserDexAbstraction {
            user: target.into(),
            enabled: true,
            agent: false,
        },
    ] {
        assert!(
            HyperliquidActionEnvelope {
                nonce_ms: nonce,
                vault_address: Some(target.into()),
                action,
            }
            .action_wire_json()
            .is_err(),
            "user-signed financial or authority actions cannot use L1 vault scope"
        );
    }
}

#[tokio::test]
async fn builder_order_signature_matches_independent_messagepack_eip712_vector() {
    let key = format!("0x{:064x}", 1);
    let address = "0x2222222222222222222222222222222222222222";
    let nonce = 1_700_000_000_000u64;
    let action_model = HyperliquidAction::Order {
        orders: vec![HyperliquidOrder {
            asset: 0,
            is_buy: true,
            price: "123.45".into(),
            size: "0.0123".into(),
            reduce_only: false,
            order_type: HyperliquidOrderType::Limit {
                time_in_force: HyperliquidTimeInForce::Gtc,
            },
            client_order_id: Some("0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into()),
        }],
        grouping: HyperliquidOrderGrouping::Na,
        builder_fee: Some(HyperliquidBuilderFee {
            address: address.into(),
            fee_tenths_bps: 75,
        }),
    };
    let mut core = HyperliquidCore::new(Some(ccxt::Value::from(
        serde_json::json!({"options":{"sandboxMode":false}}),
    )));
    core.privateKey = ccxt::Value::str(key);
    let action = hl_action_value(&action_model, nonce).unwrap();
    let signed = ccxt::runtime::call_typed(core.call_dynamic(
        "sign_l1_action",
        vec![action, ccxt::Value::Int(nonce as i64), ccxt::Value::Null],
    ))
    .await
    .unwrap();
    let signature = signed.as_map().unwrap();
    let r: [u8; 32] = hex::decode(signature["r"].as_str().unwrap().trim_start_matches("0x"))
        .unwrap()
        .try_into()
        .unwrap();
    let s: [u8; 32] = hex::decode(signature["s"].as_str().unwrap().trim_start_matches("0x"))
        .unwrap()
        .try_into()
        .unwrap();
    let sig = Signature::from_scalars(r, s).unwrap();
    let recovery = RecoveryId::try_from((signature["v"].as_i64().unwrap() - 27) as u8).unwrap();

    let mp_map = |entries: Vec<(&str, MpValue)>| {
        MpValue::Map(
            entries
                .into_iter()
                .map(|(key, value)| (MpValue::String(key.into()), value))
                .collect(),
        )
    };
    let action_msgpack = mp_map(vec![
        ("type", MpValue::String("order".into())),
        (
            "orders",
            MpValue::Array(vec![mp_map(vec![
                ("a", MpValue::from(0)),
                ("b", MpValue::Boolean(true)),
                ("p", MpValue::String("123.45".into())),
                ("s", MpValue::String("0.0123".into())),
                ("r", MpValue::Boolean(false)),
                (
                    "t",
                    mp_map(vec![(
                        "limit",
                        mp_map(vec![("tif", MpValue::String("Gtc".into()))]),
                    )]),
                ),
                (
                    "c",
                    MpValue::String("0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into()),
                ),
            ])]),
        ),
        ("grouping", MpValue::String("na".into())),
        (
            "builder",
            mp_map(vec![
                ("b", MpValue::String(address.into())),
                ("f", MpValue::from(75)),
            ]),
        ),
    ]);
    let mut bytes = Vec::new();
    rmpv::encode::write_value(&mut bytes, &action_msgpack).unwrap();
    bytes.extend_from_slice(&nonce.to_be_bytes());
    bytes.push(0); // no vault address
    let connection_id = keccak(&bytes);
    let domain_type = keccak(
        b"EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)",
    );
    let mut domain = Vec::new();
    domain.extend_from_slice(&domain_type);
    domain.extend_from_slice(&keccak(b"Exchange"));
    domain.extend_from_slice(&keccak(b"1"));
    domain.extend_from_slice(&word_u64(1337));
    domain.extend_from_slice(&[0u8; 32]);
    let domain_hash = keccak(&domain);
    let agent_type_hash = keccak(b"Agent(string source,bytes32 connectionId)");
    let mut agent = Vec::new();
    agent.extend_from_slice(&agent_type_hash);
    agent.extend_from_slice(&keccak(b"a"));
    agent.extend_from_slice(&connection_id);
    let agent_hash = keccak(&agent);
    let mut digest_input = vec![0x19, 0x01];
    digest_input.extend_from_slice(&domain_hash);
    digest_input.extend_from_slice(&agent_hash);
    let digest = keccak(&digest_input);
    let recovered = VerifyingKey::recover_from_prehash(&digest, &sig, recovery).unwrap();
    let public = recovered.to_encoded_point(false);
    let address_hash = keccak(&public.as_bytes()[1..]);
    assert_eq!(
        hex::encode(&address_hash[12..]),
        "7e5f4552091a69125d5dfcb7b8c2659029395bdf"
    );
}

#[test]
fn isolated_margin_direction_is_signed_micro_usdc_not_is_buy() {
    for (amount, micro) in [("2.5", 2_500_000), ("-2.5", -2_500_000)] {
        let wire = ccxt_value_to_json(
            hl_action_value(
                &HyperliquidAction::UpdateIsolatedMargin {
                    asset: 1,
                    amount_usdc: amount.into(),
                },
                1,
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            wire,
            serde_json::json!({"type":"updateIsolatedMargin","asset":1,"isBuy":true,"ntli":micro})
        );
    }
    assert!(
        hl_action_value(
            &HyperliquidAction::UpdateIsolatedMargin {
                asset: 1,
                amount_usdc: "-0.0000001".into()
            },
            1
        )
        .is_err()
    );
}

#[test]
fn agent_abstraction_uses_l1_native_short_modes_and_enable_action() {
    let owner = "0x1111111111111111111111111111111111111111";
    for (mode, encoded) in [
        (HyperliquidAbstraction::Disabled, "i"),
        (HyperliquidAbstraction::UnifiedAccount, "u"),
        (HyperliquidAbstraction::PortfolioMargin, "p"),
    ] {
        let action = HyperliquidAction::UserAbstraction {
            user: owner.into(),
            abstraction: mode,
            agent: true,
        };
        let wire = ccxt_value_to_json(hl_action_value(&action, 1).unwrap()).unwrap();
        assert_eq!(
            wire,
            serde_json::json!({"type":"agentSetAbstraction","abstraction":encoded})
        );
    }
    let action = HyperliquidAction::UserDexAbstraction {
        user: owner.into(),
        enabled: true,
        agent: true,
    };
    assert_eq!(
        ccxt_value_to_json(hl_action_value(&action, 1).unwrap()).unwrap(),
        serde_json::json!({"type":"agentEnableDexAbstraction"})
    );
    assert!(
        hl_action_value(
            &HyperliquidAction::UserDexAbstraction {
                user: owner.into(),
                enabled: false,
                agent: true
            },
            1
        )
        .is_err()
    );
}
