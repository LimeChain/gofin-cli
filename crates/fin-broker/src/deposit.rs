//! Protected composite for EVM asset deposits into a Binance account. It
//! exposes no arbitrary endpoint or recipient; the destination comes from a
//! fresh, account-authenticated Binance metadata/address read.

use async_trait::async_trait;
use fin_binance::BinanceAdapter;
use fin_domain::{
    Adapter, Amount, AssetId, Check, CheckStatus, ConnectionMetadata, Definition, EffectConstraint,
    ExecutionActor, FinError, FinancialEffect, FinancialIntent, FinancialState, OperationKind,
    OperationRecord, PreparedOperation, Quantity, Reconciliation, ResourceAmount, ResourceId,
    ReturnRouteContext, ReturnRouteProof, StateSnapshot, Status, Submission, binding_hash, now,
    verified_effect_digest,
};
use fin_ethereum::{BrokerVerifiedNativeDepositTarget, EthereumAdapter};
use schemars::{JsonSchema, schema_for};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;

#[path = "deposit_presentation.rs"]
mod presentation;

fn error(code: &str, stage: &str, message: &str, next: &str) -> FinError {
    FinError::new(code, stage, message, next)
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct DepositInput {
    request_id: String,
    account: String,
    wallet: String,
    amount: String,
    max_fee_eth: String,
    #[serde(default)]
    coin: Option<String>,
    #[serde(default)]
    network: Option<String>,
    #[serde(default)]
    chain_id: Option<u64>,
    #[serde(default)]
    token: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct DepositBinding {
    target: BrokerVerifiedNativeDepositTarget,
    inner_input: Value,
    inner_preview: PreparedOperation,
    source_free_before: String,
    #[serde(default)]
    coin: String,
    #[serde(default)]
    source_asset: Option<AssetId>,
    #[serde(
        default,
        rename = "_fin_usd_ceiling_v1",
        skip_serializing_if = "Option::is_none"
    )]
    usd_ceiling: Option<Value>,
}

/// Broker-only composition over the existing fixed Binance and Ethereum
/// adapters. Construct this only after both adapters have protected signers.
pub struct BinanceEthereumDepositAdapter {
    binance: Arc<BinanceAdapter>,
    ethereum: std::collections::BTreeMap<u64, Arc<EthereumAdapter>>,
}

impl BinanceEthereumDepositAdapter {
    pub fn new(binance: Arc<BinanceAdapter>, ethereum: Arc<EthereumAdapter>) -> Self {
        let chain_id = ethereum.config.chain_id;
        Self {
            binance,
            ethereum: std::collections::BTreeMap::from([(chain_id, ethereum)]),
        }
    }

    pub fn with_networks(
        binance: Arc<BinanceAdapter>,
        ethereum: impl IntoIterator<Item = Arc<EthereumAdapter>>,
    ) -> Self {
        Self {
            binance,
            ethereum: ethereum
                .into_iter()
                .map(|adapter| (adapter.config.chain_id, adapter))
                .collect(),
        }
    }

    fn input(raw: &Value) -> Result<DepositInput, FinError> {
        serde_json::from_value(raw.clone()).map_err(|_| {
            error(
                "INVALID_INPUT",
                "validation",
                "Binance deposit input is malformed or contains unknown fields.",
                "Use the binance.deposit schema.",
            )
        })
    }

    fn account(&self, name: &str) -> Result<ConnectionMetadata, FinError> {
        self.binance
            .connections()
            .into_iter()
            .find(|connection| {
                connection.name == name
                    && matches!(connection.role.as_deref(), Some("main" | "sub"))
            })
            .ok_or_else(|| {
                error(
                    "BINANCE_ACCOUNT_UNKNOWN",
                    "validation",
                    "Deposit credit must target an enrolled Binance main or subaccount.",
                    "Select a configured Binance account with its own valid credentials.",
                )
            })
    }

    fn wallet(
        &self,
        name: &str,
        chain_id: u64,
    ) -> Result<(Arc<EthereumAdapter>, ConnectionMetadata), FinError> {
        let ethereum = self.ethereum.get(&chain_id).cloned().ok_or_else(|| {
            error(
                "EVM_NETWORK_UNAVAILABLE",
                "validation",
                "Selected EVM chain is not configured in Fin.",
                "Choose a configured Ethereum, Arbitrum One, or Robinhood Chain network.",
            )
        })?;
        let wallet = ethereum
            .connections()
            .into_iter()
            .find(|connection| {
                connection.name == name
                    && connection.network.as_deref() == Some(&format!("ethereum:{chain_id}"))
            })
            .ok_or_else(|| {
                error(
                    "ETHEREUM_WALLET_UNKNOWN",
                    "validation",
                    "Selected wallet is not enrolled on the selected EVM chain.",
                    "Select a configured signing wallet on this chain.",
                )
            })?;
        Ok((ethereum, wallet))
    }

    fn input_route(input: &DepositInput) -> Result<(String, String, u64), FinError> {
        let coin = input.coin.clone().unwrap_or_else(|| "ETH".into());
        let network = input.network.clone().unwrap_or_else(|| "ETH".into());
        let inferred = fin_binance::binance_evm_chain_id(&network).unwrap_or(0);
        let chain_id = input.chain_id.unwrap_or(inferred);
        if coin.is_empty()
            || coin.len() > 32
            || network.is_empty()
            || network.len() > 64
            || chain_id == 0
        {
            return Err(error(
                "DEPOSIT_ROUTE_UNSUPPORTED",
                "validation",
                "The selected Binance coin/network is not a supported Fin EVM route.",
                "Choose a route shown by Binance metadata on a configured Fin EVM network.",
            ));
        }
        let expected = fin_binance::binance_evm_chain_id(&network);
        if expected != Some(chain_id) || ![1, 42161, 4663].contains(&chain_id) {
            return Err(error(
                "DEPOSIT_ROUTE_UNSUPPORTED",
                "validation",
                "The Binance network does not match a supported Fin EVM chain.",
                "Choose ETH, Arbitrum One, or Robinhood Chain using its current Binance network code.",
            ));
        }
        Ok((coin, network, chain_id))
    }

    async fn target(
        &self,
        account_name: &str,
        wallet_name: &str,
        coin: &str,
        network_name: &str,
        chain_id: u64,
        token_request: Option<&str>,
    ) -> Result<(BrokerVerifiedNativeDepositTarget, AssetId), FinError> {
        let account = self.account(account_name)?;
        let networks = self
            .binance
            .read(
                "binance.networks",
                &json!({"account":account_name,"coin":coin}),
            )
            .await?;
        let eth_asset = networks["assets"]
            .as_array()
            .and_then(|assets| {
                let matches = assets
                    .iter()
                    .filter(|asset| asset["coin"] == coin)
                    .collect::<Vec<_>>();
                (matches.len() == 1).then(|| matches[0])
            })
            .ok_or_else(|| {
                error(
                    "DEPOSIT_NETWORK_UNAVAILABLE",
                    "preview",
                    "Binance did not return one unambiguous record for the selected coin.",
                    "Retry after reviewing the venue's current funding network.",
                )
            })?;
        let network = eth_asset["networks"]
            .as_array()
            .and_then(|networks| {
                let matches = networks
                    .iter()
                    .filter(|network| network["network"] == network_name)
                    .collect::<Vec<_>>();
                (matches.len() == 1).then(|| matches[0])
            })
            .ok_or_else(|| {
                error(
                    "DEPOSIT_NETWORK_UNAVAILABLE",
                    "preview",
                    "Binance did not return one unambiguous selected network record.",
                    "Select the exact current network code returned by Binance.",
                )
            })?;
        if eth_asset["depositAllEnable"] != true
            || network["depositEnable"] != true
            || network["coin"] != coin
        {
            return Err(error(
                "DEPOSIT_DISABLED",
                "preview",
                "Binance does not currently accept deposits for this exact coin and network.",
                "Wait until Binance enables this route or choose an enabled route.",
            ));
        }
        // Binance's documented depositDust field is the minimum creditable
        // amount. Missing values fail closed; it is never inferred locally.
        let minimum_decimal = network["depositDust"].as_str().ok_or_else(|| {
            error(
                "DEPOSIT_MINIMUM_UNKNOWN",
                "preview",
                "Binance did not provide its current minimum creditable amount for this route.",
                "Do not send funds until Binance provides an exact minimum.",
            )
        })?;
        let minimum_deposit = Quantity::from_decimal(minimum_decimal, 36).map_err(|_| {
            error(
                "DEPOSIT_MINIMUM_INVALID",
                "preview",
                "Binance's deposit minimum is not an exact supported decimal.",
                "Review the venue response before funding.",
            )
        })?;
        let contract = network["contractAddress"].as_str().unwrap_or("");
        let is_native = contract.is_empty() || contract == "unknown";
        let address_result = self
            .binance
            .read(
                "binance.deposit-address",
                &json!({"account":account_name,"coin":coin,"network":network_name}),
            )
            .await?;
        let address = address_result["address"].as_str().ok_or_else(|| {
            error(
                "DEPOSIT_ADDRESS_INVALID",
                "preview",
                "Binance did not return an exact deposit address.",
                "Refresh the address through the protected venue adapter.",
            )
        })?;
        let address_contract = address_result["tokenContract"]
            .as_str()
            .unwrap_or("unknown");
        if !is_native
            && (address_contract == "unknown" || !address_contract.eq_ignore_ascii_case(contract))
        {
            return Err(error(
                "ASSET_IDENTITY_MISMATCH",
                "preview",
                "Binance's address endpoint and network metadata disagree about the token contract.",
                "Refresh the route later or use another verified network.",
            ));
        }
        if is_native && address_contract != "unknown" && !address_contract.is_empty() {
            return Err(error(
                "ASSET_IDENTITY_MISMATCH",
                "preview",
                "Binance's address endpoint reports a token contract for a route whose network metadata declares a native asset.",
                "Refresh the route later or use another verified native network.",
            ));
        }
        if let Some(address_minimum) = address_result["minimumDeposit"].as_str()
            && address_minimum != "unknown"
        {
            let address_minimum = Quantity::from_decimal(address_minimum, 36).map_err(|_| {
                error(
                    "DEPOSIT_MINIMUM_INVALID",
                    "preview",
                    "Binance address metadata returned an invalid minimum deposit.",
                    "Review the current venue route before funding.",
                )
            })?;
            if address_minimum != minimum_deposit {
                return Err(error(
                    "DEPOSIT_METADATA_MISMATCH",
                    "preview",
                    "Binance's network and address endpoints disagree about the minimum deposit.",
                    "Refresh the route after Binance metadata becomes consistent.",
                ));
            }
        }
        if address_result["coin"] != coin
            || address_result["network"] != network_name
            || address_result["memo"]
                .as_str()
                .is_none_or(|memo| !memo.is_empty())
            || address.len() != 42
            || !address.starts_with("0x")
            || !address[2..].bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(error(
                "DEPOSIT_ADDRESS_UNSUPPORTED",
                "preview",
                "Binance deposit target is invalid or requires an unsupported memo/tag.",
                "Use a plain EVM address on a route that does not require a memo or tag.",
            ));
        }
        match network.get("memoRegex") {
            Some(Value::Null) => {}
            Some(Value::String(value)) if value.is_empty() => {}
            _ => {
                return Err(error(
                    "DEPOSIT_MEMO_UNSUPPORTED",
                    "preview",
                    "Binance metadata is missing or describes a memo/tag requirement that this EVM deposit path cannot safely satisfy.",
                    "Use a route whose current metadata establishes that no memo or tag is required.",
                ));
            }
        }
        let supported_denomination = |value: &Value| {
            value.is_null() || value.as_u64() == Some(1) || value.as_str() == Some("unknown")
        };
        if !supported_denomination(&network["denomination"])
            || !supported_denomination(&address_result["denomination"])
        {
            return Err(error(
                "DEPOSIT_DENOMINATION_UNSUPPORTED",
                "preview",
                "Binance's reported denomination differs from supported one-to-one EVM token units.",
                "Do not deposit until this exact route has an explicitly supported denomination.",
            ));
        }
        let (ethereum, _) = self.wallet(wallet_name, chain_id)?;
        let source_asset = if is_native {
            if token_request.is_some_and(|token| token != "native") || coin != "ETH" {
                return Err(error(
                    "ASSET_IDENTITY_MISMATCH",
                    "preview",
                    "A native EVM route must identify Binance ETH and cannot be requested as an ERC20 token.",
                    "Use coin ETH with token native.",
                ));
            }
            AssetId {
                network: format!("ethereum:{chain_id}"),
                reference: "native".into(),
                decimals: 18,
            }
        } else {
            if !contract.starts_with("0x")
                || contract.len() != 42
                || !contract[2..].bytes().all(|byte| byte.is_ascii_hexdigit())
            {
                return Err(error(
                    "DEPOSIT_TOKEN_IDENTITY_INVALID",
                    "preview",
                    "Binance contract metadata is not a valid EVM token address.",
                    "Use another deposit network with a valid exact contract address.",
                ));
            }
            if !token_request.is_some_and(|requested| requested.eq_ignore_ascii_case(contract)) {
                return Err(error(
                    "ASSET_IDENTITY_MISMATCH",
                    "preview",
                    "The request token must exactly match Binance's current contract metadata for this coin and network.",
                    "Pass the exact token contract returned by Binance network metadata.",
                ));
            }
            let token_response = ethereum.read("wallet.token", &json!({"wallet":wallet_name,"network":"evm","chain_id":chain_id,"token":contract})).await?;
            let token_data = &token_response["data"];
            let decimals = token_data["decimals"]
                .as_u64()
                .filter(|decimals| *decimals <= 36)
                .ok_or_else(|| {
                    error(
                        "DEPOSIT_TOKEN_METADATA_INVALID",
                        "preview",
                        "The EVM token returned invalid denomination metadata.",
                        "Choose a supported token with exact on-chain decimals.",
                    )
                })? as u8;
            let reference = token_data["reference"].as_str().ok_or_else(|| {
                error(
                    "DEPOSIT_TOKEN_METADATA_INVALID",
                    "preview",
                    "The EVM token identity is missing.",
                    "Retry after reviewing the token contract.",
                )
            })?;
            if !reference.eq_ignore_ascii_case(contract) {
                return Err(error(
                    "ASSET_IDENTITY_MISMATCH",
                    "preview",
                    "The on-chain token identity differs from Binance contract metadata.",
                    "Do not send this asset through the selected route.",
                ));
            }
            AssetId {
                network: format!("ethereum:{chain_id}"),
                reference: contract.to_ascii_lowercase(),
                decimals,
            }
        };
        let target_evidence = json!({
            "accountResource":account.resource,
            "coin":coin,
            "network":network_name,
            "chainId":chain_id,
            "contract":contract,
            "denomination":1,
            "address":address.to_ascii_lowercase(),
            "memo":"",
            "minimumDeposit":minimum_deposit,
            "minimumConfirmations":address_result["minimumConfirmations"],
            "unlockConfirmations":address_result["unlockConfirmations"],
            "depositEnabled":true,
            "minimumSource":"Binance capital config depositDust (minimum creditable amount)"
        });
        let evidence_digest = binding_hash(&target_evidence).map_err(|_| {
            error(
                "DEPOSIT_TARGET_INVALID",
                "preview",
                "Binance deposit evidence could not be canonically bound.",
                "Retry the protected funding read.",
            )
        })?;
        let observed_at = now();
        let expires_at = chrono::DateTime::parse_from_rfc3339(&observed_at)
            .map_err(|_| {
                error(
                    "CLOCK_INVALID",
                    "preview",
                    "System UTC is invalid.",
                    "Retry.",
                )
            })?
            .with_timezone(&chrono::Utc)
            .checked_add_signed(chrono::Duration::seconds(90))
            .ok_or_else(|| {
                error(
                    "CLOCK_INVALID",
                    "preview",
                    "Deposit target expiry overflowed.",
                    "Retry.",
                )
            })?
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        Ok((
            BrokerVerifiedNativeDepositTarget {
                chain_id,
                account_resource: account.resource,
                address: address.into(),
                asset: AssetId {
                    network: "binance".into(),
                    reference: coin.into(),
                    decimals: 36,
                },
                minimum_deposit,
                network: network_name.into(),
                observed_at,
                expires_at,
                evidence_digest,
            },
            source_asset,
        ))
    }

    fn inner_input(
        input: &DepositInput,
        target: &BrokerVerifiedNativeDepositTarget,
        token: &str,
    ) -> Value {
        json!({
            "wallet":input.wallet,
            "network":"evm",
            "chain_id":target.chain_id,
            "request_id":input.request_id,
            "max_fee_eth":input.max_fee_eth,
            "token":token,
            "amount":input.amount,
            "recipient":target.address
        })
    }

    async fn source_free(&self, account: &str, coin: &str) -> Result<String, FinError> {
        let balances = self
            .binance
            .read("binance.balances", &json!({"account":account}))
            .await?;
        let rows = balances["balances"].as_array().ok_or_else(|| {
            error(
                "BALANCE_OBSERVATION_INVALID",
                "preview",
                "Binance did not return its exact SPOT balance set.",
                "Refresh the protected account balance.",
            )
        })?;
        let assets = rows
            .iter()
            .filter(|row| row["asset"] == coin)
            .collect::<Vec<_>>();
        if assets.len() != 1 {
            return Err(error(
                "BALANCE_OBSERVATION_INVALID",
                "preview",
                "Selected Binance coin balance is missing or ambiguous.",
                "Refresh the protected account balance.",
            ));
        }
        let free = assets[0]["free"].as_str().ok_or_else(|| {
            error(
                "BALANCE_OBSERVATION_INVALID",
                "preview",
                "Selected Binance coin free balance is not an exact decimal string.",
                "Do not continue with rounded balances.",
            )
        })?;
        Quantity::from_decimal(free, 36).map_err(|_| {
            error(
                "BALANCE_OBSERVATION_INVALID",
                "preview",
                "Selected Binance coin free balance exceeds supported exact precision.",
                "Do not continue with rounded balances.",
            )
        })?;
        Ok(free.into())
    }

    fn cross_custody_intent(
        inner: &PreparedOperation,
        target: &BrokerVerifiedNativeDepositTarget,
        amount: &str,
        source_asset: &AssetId,
    ) -> Result<FinancialIntent, FinError> {
        let source = inner
            .intent
            .debits
            .first()
            .map(|debit| debit.resource.clone())
            .ok_or_else(|| {
                error(
                    "INNER_PREVIEW_INVALID",
                    "preview",
                    "Ethereum transfer preview omitted its source resource.",
                    "Refresh the Ethereum wallet preview.",
                )
            })?;
        let debit_atoms = Quantity::from_decimal(amount, source_asset.decimals).map_err(|_| {
            error(
                "INVALID_AMOUNT",
                "preview",
                "Amount exceeds the selected EVM asset's exact on-chain precision.",
                "Use an exact amount supported by the selected asset.",
            )
        })?;
        let credit_atoms = Quantity::from_decimal(amount, 36).map_err(|_| {
            error(
                "INVALID_AMOUNT",
                "preview",
                "Venue credit exceeds the exact Binance precision.",
                "Use an exact amount supported by Binance's venue precision.",
            )
        })?;
        let mut locks = vec![source.clone(), target.account_resource.clone()];
        locks.sort();
        locks.dedup();
        Ok(FinancialIntent {
            debits: vec![ResourceAmount {
                resource: source.clone(),
                amount: Amount {
                    asset: source_asset.clone(),
                    atoms: debit_atoms.clone(),
                },
            }],
            maximum_costs: inner.intent.maximum_costs.clone(),
            expected: vec![EffectConstraint::Transfer {
                from: source,
                to: target.account_resource.clone(),
                debit_asset: source_asset.clone(),
                credit_asset: target.asset.clone(),
                maximum_debit: debit_atoms,
                minimum_credit: credit_atoms,
            }],
            locks,
        })
    }

    pub fn operation_definition() -> Definition {
        Definition {
            name: "binance.deposit".into(),
            description: "Deposit native ETH or an exact ERC20 token from a protected EVM wallet into a Binance main account or subaccount using its own credentials. Live Binance metadata selects the asset/network route; completion requires canonical chain evidence and matching usable Binance credit.".into(),
            kind: OperationKind::Write,
            execution_actor: ExecutionActor::AgentInvocable,
            input_schema: serde_json::to_value(schema_for!(DepositInput)).unwrap_or(Value::Null),
            output_schema: json!({"type":"object"}),
            error_schema: schema_for!(FinError).into(),
            examples: vec![
                json!({"requestId":"deposit-native-001","account":"binance-sub","wallet":"wallet","coin":"ETH","network":"ARBITRUM","chainId":42161,"token":"native","amount":"0.001","maxFeeEth":"0.0001"}),
                json!({"requestId":"deposit-token-001","account":"binance-sub","wallet":"wallet","coin":"USDC","network":"ARBITRUM","chainId":42161,"token":"0xaf88d065e77c8cc2239327c5edb3a432268e5831","amount":"1","maxFeeEth":"0.0001"}),
            ],
            coverage: "Synthetic broker-composite transport and canonical fake-EVM proof; live funding capabilities and acceptance remain separately reported.".into(),
            completion: "Original transaction identity is canonical and finalized, and exactly one usable matching Binance credit plus the corresponding free-balance increase is observed.".into(),
            semantics: json!({"legacyDefaults":"Omitted coin/network select ETH on Ethereum mainnet; omitted chainId resolves the reviewed Binance network mapping. ERC20 token contract must be explicit.","accounts":"Main or subaccount using its own credential; no master required. Parent-authorized hierarchy reads remain optional.","sourceCoverage":"Configured Fin EVM chains with an explicit reviewed Binance network mapping; other source chains, memo/tag routes and non-unit denominations are unavailable.","approval":"Fresh Touch ID binds source wallet, destination account, chain, asset, deposit address, amount, gas envelope and displayed USD ceilings. Fresh quote evidence may vary within those ceilings.","recovery":"Reconcile the original operation after response loss, delayed credit or restart; do not resend under a new request ID.","recipient":"fixed from the authenticated account's current Binance deposit-address endpoint","coin":"Binance asset identity; never resolved from an EVM ticker","network":"explicit Binance network code matched to current metadata and a configured Fin EVM chain","token":"native or exact ERC20 contract; contract and decimals are checked against both Binance and chain metadata","amount":"exact finite amount; amount=all is unsupported","completion":"chain submission alone is insufficient; matching usable deposit history and balance delta are required","venueAssetDecimals":36,"maxFeeEth":"Maximum total chain-native fee in ETH. The wallet transfer reserves 20% gas-unit and up to 50% fee-quote headroom, including priority fees, within this cap before approval; routine gas changes within the frozen envelope are accepted.","broker_generated_attempt_input_fields":["/requestId"]}),
        }
    }

    async fn prepare_deposit(
        &self,
        operation: &str,
        raw: &Value,
        approved_inner: Option<&PreparedOperation>,
    ) -> Result<PreparedOperation, FinError> {
        let input = Self::input(raw)?;
        let account = self.account(&input.account)?;
        let (coin, network, chain_id) = Self::input_route(&input)?;
        let (ethereum, wallet) = self.wallet(&input.wallet, chain_id)?;
        if !wallet.signing_enabled || input.request_id.is_empty() || input.request_id.len() > 128 {
            return Err(error(
                "DEPOSIT_CONFIGURATION_INVALID",
                "preview",
                "Deposit requires a signing-enabled named wallet and bounded request ID.",
                "Select an enrolled signing wallet and a new request ID.",
            ));
        }
        let (target, source_asset) = self
            .target(
                &input.account,
                &input.wallet,
                &coin,
                &network,
                chain_id,
                input.token.as_deref(),
            )
            .await?;
        let amount_atoms = Quantity::from_decimal(&input.amount, source_asset.decimals).map_err(|_| {
            error(
                "INVALID_AMOUNT",
                "preview",
                "Deposit amount must be a positive exact decimal within the selected asset's precision.",
                "Use a finite exact native or ERC20 amount.",
            )
        })?;
        if amount_atoms.is_zero() {
            return Err(error(
                "INVALID_AMOUNT",
                "preview",
                "Deposit amount must be positive.",
                "Use a positive finite amount.",
            ));
        }
        let venue_atoms = Quantity::from_decimal(&input.amount, 36).map_err(|_| {
            error(
                "INVALID_AMOUNT",
                "preview",
                "Deposit amount cannot be represented exactly by Binance venue precision.",
                "Use an exact finite amount accepted by the selected route.",
            )
        })?;
        if venue_atoms < target.minimum_deposit {
            return Err(error(
                "DEPOSIT_MINIMUM",
                "preview",
                "Selected exact amount is below Binance's observed minimum creditable amount.",
                "Increase the amount or choose another supported route.",
            ));
        }
        let source_free_before = self.source_free(&input.account, &coin).await?;
        let token = source_asset.reference.as_str();
        let inner_input = Self::inner_input(&input, &target, token);
        let inner_preview = match approved_inner {
            Some(original) => ethereum.revalidate(original).await?,
            None => ethereum.prepare("wallet.transfer", &inner_input).await?,
        };
        if inner_preview
            .intent
            .debits
            .first()
            .is_none_or(|debit| debit.amount.asset != source_asset)
        {
            return Err(error(
                "ASSET_IDENTITY_MISMATCH",
                "preview",
                "The wallet transfer preview did not preserve the selected token identity.",
                "Refresh the deposit route and verify its contract and decimals.",
            ));
        }
        let intent =
            Self::cross_custody_intent(&inner_preview, &target, &input.amount, &source_asset)?;
        let summary = presentation::summary(
            &input.amount,
            &coin,
            (&wallet.name, wallet.address.as_deref().unwrap_or("unknown")),
            (&account.name, &target.address),
            chain_id,
            &input.max_fee_eth,
        );
        let binding = DepositBinding {
            target,
            inner_input,
            inner_preview: inner_preview.clone(),
            source_free_before,
            coin: coin.clone(),
            source_asset: Some(source_asset.clone()),
            usd_ceiling: None,
        };
        let mut prepared = PreparedOperation {
            operation: operation.into(),
            input: raw.clone(),
            binding: serde_json::to_value(binding).map_err(|_| internal_error())?,
            summary,
            checks: vec![Check { name:"venue_deposit_enabled".into(), status:CheckStatus::Passed, detail:format!("Binance metadata confirms {coin} deposits are enabled on {network} and supplies an exact minimum.") }, Check { name:"asset_identity".into(), status:CheckStatus::Passed, detail:format!("Coin {coin} is bound to exact EVM asset {} with {} decimals; denomination and memo requirements are supported.", source_asset.reference, source_asset.decimals) }, Check { name:"evm_transfer_preview".into(), status:CheckStatus::Passed, detail:"Ordinary wallet.transfer froze the exact finite value, source wallet, recipient, chain and fee cap.".into() }],
            warnings: vec!["A successful Ethereum transaction does not complete this operation until Binance records one usable credited deposit.".into()],
            observed_at: inner_preview.observed_at.clone(),
            expires_at: inner_preview.expires_at.clone(),
            valuation: inner_preview.valuation.clone(),
            intent,
            operation_version: 2,
        };
        // Compose the wallet's approved USD ceilings without changing its exact
        // principal or gas envelope. Legacy inner previews retain exact pricing.
        if approved_inner.is_none()
            || inner_preview
                .binding
                .get(fin_domain::value_bounds::KEY)
                .is_some()
        {
            fin_domain::value_bounds::bind(&mut prepared);
        }
        Ok(prepared)
    }
}

fn internal_error() -> FinError {
    error(
        "INTERNAL_ERROR",
        "broker",
        "Protected deposit record is invalid.",
        "Inspect the protected broker diagnostics.",
    )
}

#[async_trait]
impl Adapter for BinanceEthereumDepositAdapter {
    fn definitions(&self) -> Vec<Definition> {
        vec![Self::operation_definition()]
    }

    fn connections(&self) -> Vec<ConnectionMetadata> {
        // Deposit-route metadata is an authorization identity, not a separate
        // public connection. Public discovery lists the enrolled account and
        // grouped EVM wallet through their owning adapters.
        Vec::new()
    }

    fn authorization_identities(&self) -> Vec<ConnectionMetadata> {
        // Bind the receiving Binance accounts, configured deposit routes and
        // per-chain EVM signer details without leaking internal destination
        // rows into `fin connections list`.
        let mut identities = self.binance.authorization_identities();
        for adapter in self.ethereum.values() {
            for identity in adapter.authorization_identities() {
                if !identities.contains(&identity) {
                    identities.push(identity);
                }
            }
        }
        identities
    }

    async fn snapshot(&self) -> Result<StateSnapshot, FinError> {
        Ok(StateSnapshot {
            state: FinancialState::default(),
            observed_at: now(),
            complete: true,
            coverage: Vec::new(),
            pending: Vec::new(),
        })
    }

    async fn read(&self, _operation: &str, _input: &Value) -> Result<Value, FinError> {
        Err(error(
            "WRONG_OPERATION_KIND",
            "read",
            "Deposit is a write operation.",
            "Use preview before an authorized execute.",
        ))
    }

    async fn prepare(&self, operation: &str, input: &Value) -> Result<PreparedOperation, FinError> {
        if operation != "binance.deposit" {
            return Err(error(
                "UNSUPPORTED_OPERATION",
                "preview",
                "Composite supports only metadata-verified Binance deposits over Fin EVM networks.",
                "Select binance.deposit with a supported coin and network.",
            ));
        }
        self.prepare_deposit(operation, input, None).await
    }

    async fn prepare_with_context(
        &self,
        operation: &str,
        input: &Value,
        _context: &fin_domain::PreparationContext,
    ) -> Result<PreparedOperation, FinError> {
        self.prepare(operation, input).await
    }

    async fn preflight_return_route(
        &self,
        operation: &str,
        raw: &Value,
        context: &ReturnRouteContext,
    ) -> Result<ReturnRouteProof, FinError> {
        if operation != "binance.deposit" {
            return Err(error(
                "UNSUPPORTED_OPERATION",
                "preflight",
                "Composite supports only metadata-verified Binance deposits over Fin EVM networks.",
                "Select binance.deposit with a supported coin and network.",
            ));
        }
        let input = Self::input(raw)?;
        let (coin, network, chain_id) = Self::input_route(&input)?;
        let (ethereum, wallet) = self.wallet(&input.wallet, chain_id)?;
        let (target, source_asset) = self
            .target(
                &input.account,
                &input.wallet,
                &coin,
                &network,
                chain_id,
                input.token.as_deref(),
            )
            .await?;
        if !wallet.signing_enabled {
            return Err(error(
                "PROTECTED_SIGNER_REQUIRED",
                "preflight",
                "Selected wallet is not signing-enabled.",
                "Choose a protected signing wallet on this chain.",
            ));
        }
        let inner_input = Self::inner_input(&input, &target, &source_asset.reference);
        let mut route_context = context.clone();
        if !route_context
            .projected
            .cash
            .iter()
            .any(|cash| cash.resource == target.account_resource && cash.asset == target.asset)
        {
            let observed_free = self.source_free(&input.account, &coin).await?;
            let exact_free = Quantity::from_decimal(&observed_free, target.asset.decimals)
                .map_err(|_| {
                    error(
                        "BALANCE_INVALID",
                        "preflight",
                        "Current Binance free balance is not an exact supported decimal.",
                        "Retry after reviewing the SPOT balance response.",
                    )
                })?;
            route_context
                .projected
                .cash
                .push(fin_domain::ProjectedCash {
                    resource: target.account_resource.clone(),
                    asset: target.asset.clone(),
                    amount: fin_domain::QuantityInterval {
                        minimum: exact_free.clone(),
                        maximum: exact_free,
                    },
                });
        }
        let mut proof = ethereum
            .preflight_deposit_transfer(&inner_input, &route_context, &target, &source_asset)
            .await?;
        proof.operation = operation.into();
        proof.input_hash = binding_hash(raw)?;
        proof.evidence["composite"] = json!({"operation":operation,"account":input.account,"wallet":input.wallet,"targetEvidenceDigest":target.evidence_digest,"completion":"canonical transaction plus exact matching usable venue credit required"});
        Ok(proof)
    }

    async fn revalidate(
        &self,
        prepared: &PreparedOperation,
    ) -> Result<PreparedOperation, FinError> {
        if prepared.operation != "binance.deposit" {
            return Err(error(
                "UNSUPPORTED_OPERATION",
                "submission",
                "Unexpected composite operation.",
                "Preview binance.deposit again.",
            ));
        }
        if prepared.operation_version != 2 {
            return Err(error(
                "PREVIEW_CHANGED",
                "submission",
                "Historical deposit previews cannot authorize a new submission after this upgrade.",
                "Reconcile already submitted operations by their original ID; unsubmitted work needs a fresh preview and approval.",
            ));
        }
        let original: DepositBinding =
            serde_json::from_value(prepared.binding.clone()).map_err(|_| internal_error())?;
        // Refresh Binance route, destination and source balance while asking
        // Ethereum to revalidate the approved transfer envelope in place.
        // A new gas quote must not silently replace the envelope being approved.
        let fresh = self
            .prepare_deposit(
                "binance.deposit",
                &prepared.input,
                Some(&original.inner_preview),
            )
            .await?;
        let current: DepositBinding =
            serde_json::from_value(fresh.binding.clone()).map_err(|_| internal_error())?;
        if original.target.chain_id != current.target.chain_id
            || original.target.account_resource != current.target.account_resource
            || !original
                .target
                .address
                .eq_ignore_ascii_case(&current.target.address)
            || original.target.asset != current.target.asset
            || original.target.minimum_deposit != current.target.minimum_deposit
            || original.target.network != current.target.network
            || original.target.evidence_digest != current.target.evidence_digest
            || original.source_asset != current.source_asset
            || original.coin != current.coin
            || original.inner_input != current.inner_input
            || original.inner_preview.binding != current.inner_preview.binding
            || original.source_free_before != current.source_free_before
        {
            return Err(error(
                "PREVIEW_CHANGED",
                "submission",
                "Binance target, asset identity, source balance, or EVM transfer binding changed.",
                "Obtain a fresh preview and approval.",
            ));
        }
        let mut fresh = fresh;
        fin_domain::value_bounds::carry(prepared, &mut fresh)?;
        // Keep the approved immutable target evidence in the returned binding;
        // fresh timestamp fields are used for expiry only and never replace it.
        fresh.binding = prepared.binding.clone();
        Ok(fresh)
    }

    async fn submit(
        &self,
        prepared: &PreparedOperation,
        journal: Arc<dyn fin_domain::SubmissionJournal>,
    ) -> Result<Submission, FinError> {
        let fresh = self.revalidate(prepared).await?;
        let binding: DepositBinding =
            serde_json::from_value(fresh.binding.clone()).map_err(|_| internal_error())?;
        let ethereum = self.ethereum.get(&binding.target.chain_id).ok_or_else(|| {
            error(
                "EVM_NETWORK_UNAVAILABLE",
                "submission",
                "The approved EVM network is no longer configured.",
                "Restore that protected network configuration and obtain a fresh preview.",
            )
        })?;
        ethereum.submit(&binding.inner_preview, journal).await
    }

    async fn reconcile(&self, record: &OperationRecord) -> Result<Reconciliation, FinError> {
        let unknown = |evidence: Value| Reconciliation {
            status: Status::Unknown,
            evidence,
            verified: None,
        };
        let Some(submission) = record.submission.clone() else {
            return Ok(unknown(
                json!({"reason":"Original EVM transaction identity was not durably recorded."}),
            ));
        };
        let Some(prepared) = record.preview.clone() else {
            return Ok(unknown(
                json!({"reason":"Original composite preview is missing."}),
            ));
        };
        if !matches!(prepared.operation_version, 1 | 2) {
            return Ok(unknown(
                json!({"reason":"Persisted deposit operation version is newer than this broker understands; the original write was not replayed.","operationVersion":prepared.operation_version}),
            ));
        }
        let binding: DepositBinding = match serde_json::from_value(prepared.binding.clone()) {
            Ok(value) => value,
            Err(_) => {
                return Ok(unknown(
                    json!({"reason":"Persisted protected deposit binding is malformed."}),
                ));
            }
        };
        let inner_record = OperationRecord {
            operation: "wallet.transfer".into(),
            input: binding.inner_input.clone(),
            preview: Some(binding.inner_preview.clone()),
            ..record.clone()
        };
        let Some(ethereum) = self.ethereum.get(&binding.target.chain_id) else {
            return Ok(unknown(
                json!({"reason":"The original EVM network is no longer configured; the durable submission was not replayed.","chainId":binding.target.chain_id}),
            ));
        };
        let chain = ethereum.reconcile(&inner_record).await?;
        if chain.status != Status::Completed {
            return Ok(Reconciliation {
                status: chain.status,
                evidence: json!({"chain":chain.evidence,"venueCredit":"not yet proved"}),
                verified: chain.verified,
            });
        }
        let Some(mut verified) = chain.verified else {
            return Ok(unknown(
                json!({"reason":"Canonical Ethereum completion did not include verified financial effects."}),
            ));
        };
        if verified.submission != submission {
            return Ok(unknown(
                json!({"reason":"Ethereum proof submission differs from the original persisted identity."}),
            ));
        }
        let tx_hash = submission.reference.clone();
        let input = Self::input(&record.input)?;
        let coin = if binding.coin.is_empty() {
            input.coin.as_deref().unwrap_or("ETH")
        } else {
            &binding.coin
        };
        let Ok(account) = self.account(&input.account) else {
            return Ok(unknown(
                json!({"chain":chain.evidence,"reason":"The original Binance account is no longer configured."}),
            ));
        };
        if account.resource != binding.target.account_resource {
            return Ok(unknown(
                json!({"chain":chain.evidence,"reason":"The configured Binance account resource differs from the persisted deposit target."}),
            ));
        }
        let history = match self
            .binance
            .read(
                "binance.deposit-history",
                &json!({"account":input.account,"coin":coin,"txHash":tx_hash}),
            )
            .await
        {
            Ok(value) => value,
            Err(error) => {
                return Ok(unknown(
                    json!({"chain":chain.evidence,"venueHistoryError":error.code}),
                ));
            }
        };
        if history["complete"] != true {
            return Ok(unknown(
                json!({"chain":chain.evidence,"venueHistory":"bounded history incomplete"}),
            ));
        }
        let records = history["records"].as_array().cloned().unwrap_or_default();
        if records.len() != 1 {
            return Ok(unknown(
                json!({"chain":chain.evidence,"matchingVenueRows":records.len()}),
            ));
        }
        let row = &records[0];
        if row["creditState"] != "credited"
            || row["status"].as_u64() != Some(1)
            || row["coin"] != coin
            || row["network"] != binding.target.network
            || !row["address"]
                .as_str()
                .is_some_and(|address| address.eq_ignore_ascii_case(&binding.target.address))
            || !row["txId"]
                .as_str()
                .is_some_and(|hash| hash.eq_ignore_ascii_case(&tx_hash))
            || !(row["addressTag"].is_null()
                || row["addressTag"].as_str().is_some_and(str::is_empty))
        {
            return Ok(unknown(
                json!({"chain":chain.evidence,"venueRow":row,"reason":"Deposit row is not one exact usable credit for the original transaction and target."}),
            ));
        }
        let credit = row["amount"].as_str().ok_or_else(internal_error)?;
        let credited_atoms = Quantity::from_decimal(credit, 36).map_err(|_| internal_error())?;
        if credited_atoms < binding.target.minimum_deposit {
            return Ok(unknown(
                json!({"chain":chain.evidence,"venueRow":row,"reason":"Usable venue credit is below the current protected deposit floor."}),
            ));
        }
        let after = match self.source_free(&input.account, coin).await {
            Ok(value) => value,
            Err(error) => {
                return Ok(unknown(
                    json!({"chain":chain.evidence,"balanceError":error.code}),
                ));
            }
        };
        let before_atoms = Quantity::from_decimal(&binding.source_free_before, 36)
            .map_err(|_| internal_error())?;
        let after_atoms = Quantity::from_decimal(&after, 36).map_err(|_| internal_error())?;
        let before_integer = before_atoms.integer();
        let after_integer = after_atoms.integer();
        if after_integer < before_integer {
            return Ok(unknown(
                json!({"chain":chain.evidence,"venueRow":row,"sourceFreeBefore":binding.source_free_before,"sourceFreeAfter":after,"reason":"Binance free balance did not increase by the venue deposit credit."}),
            ));
        }
        let actual_delta = after_integer - before_integer;
        if actual_delta != credited_atoms.integer() {
            return Ok(unknown(
                json!({"chain":chain.evidence,"venueRow":row,"sourceFreeBefore":binding.source_free_before,"sourceFreeAfter":after,"reason":"Usable history amount and exact Binance free-balance increase do not match."}),
            ));
        }
        let mut effects = Vec::new();
        let mut transfer_seen = false;
        let expected_source = binding
            .inner_preview
            .intent
            .debits
            .first()
            .map(|debit| debit.resource.clone())
            .ok_or_else(internal_error)?;
        let expected_destination = ResourceId(format!(
            "ethereum:{}:{}",
            binding.target.chain_id,
            binding.target.address.to_ascii_lowercase()
        ));
        let expected_asset = binding.source_asset.clone().unwrap_or_else(|| AssetId {
            network: "ethereum:1".into(),
            reference: "native".into(),
            decimals: 18,
        });
        for effect in verified.effects.drain(..) {
            match effect {
                FinancialEffect::Transfer {
                    from,
                    to,
                    debit,
                    credit: chain_credit,
                } if !transfer_seen => {
                    let expected_credit =
                        Quantity::from_decimal(&debit.atoms.to_decimal(debit.asset.decimals), 36)
                            .map_err(|_| internal_error())?;
                    if from != expected_source
                        || to != expected_destination
                        || debit.asset != expected_asset
                        || chain_credit.asset != expected_asset
                        || chain_credit.atoms != debit.atoms
                        || binding.inner_preview.intent.debits[0].amount.atoms != debit.atoms
                        || expected_credit != credited_atoms
                    {
                        return Ok(unknown(
                            json!({"chain":chain.evidence,"venueRow":row,"reason":"Venue credited quantity does not exactly match the original EVM asset transfer principal."}),
                        ));
                    }
                    transfer_seen = true;
                    effects.push(FinancialEffect::Transfer {
                        from,
                        to: binding.target.account_resource.clone(),
                        debit,
                        credit: Amount {
                            asset: binding.target.asset.clone(),
                            atoms: credited_atoms.clone(),
                        },
                    });
                }
                FinancialEffect::Cost { .. } => effects.push(effect),
                _ => {
                    return Ok(unknown(
                        json!({"chain":chain.evidence,"reason":"Unexpected extra chain effect is not safe to translate into venue custody."}),
                    ));
                }
            }
        }
        if !transfer_seen {
            return Ok(unknown(
                json!({"chain":chain.evidence,"reason":"Canonical chain proof omitted the expected asset transfer."}),
            ));
        }
        verified.effects = effects;
        verified.evidence = json!({"method":"evm-canonical-and-binance-usable-deposit-v2","chain":chain.evidence,"venueHistory":history,"venueBalance":{"before":binding.source_free_before,"after":after,"delta":credit},"targetEvidenceDigest":binding.target.evidence_digest,"coin":coin,"network":binding.target.network,"asset":expected_asset,"originalSubmission":submission});
        verified.observed_at = now();
        verified.proof_digest = verified_effect_digest(&verified)?;
        Ok(Reconciliation {
            status: Status::Completed,
            evidence: verified.evidence.clone(),
            verified: Some(verified),
        })
    }
}
