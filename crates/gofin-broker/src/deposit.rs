//! Protected composite for EVM and Solana asset deposits into a Binance account. It
//! exposes no arbitrary endpoint or recipient; the destination comes from a
//! fresh, account-authenticated Binance metadata/address read.

use async_trait::async_trait;
use gofin_binance::BinanceAdapter;
use gofin_domain::product_names::MetadataValueExt;
use gofin_domain::{
    Adapter, Amount, AssetId, Check, CheckStatus, ConnectionMetadata, Definition, EffectConstraint,
    ExecutionActor, FinancialEffect, FinancialIntent, FinancialState, GoFinError, OperationKind,
    OperationRecord, PreparedOperation, Quantity, Reconciliation, ResourceId, ReturnRouteContext,
    ReturnRouteProof, StateSnapshot, Status, Submission, binding_hash, now, verified_effect_digest,
};
use gofin_ethereum::{BrokerVerifiedNativeDepositTarget, EthereumAdapter};
use schemars::{JsonSchema, schema_for};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;

#[path = "deposit_presentation.rs"]
mod presentation;

fn error(code: &str, stage: &str, message: &str, next: &str) -> GoFinError {
    GoFinError::new(code, stage, message, next)
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct DepositInput {
    request_id: String,
    account: String,
    wallet: String,
    amount: String,
    #[serde(default)]
    max_fee_eth: String,
    #[serde(default)]
    max_fee_sol: Option<String>,
    #[serde(default)]
    max_account_funding_sol: Option<String>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_protocol: Option<String>,
    #[serde(default)]
    coin: String,
    #[serde(default)]
    source_asset: Option<AssetId>,
    #[serde(
        default,
        rename = "_gofin_usd_ceiling_v1",
        alias = "_fin_usd_ceiling_v1",
        skip_serializing_if = "Option::is_none"
    )]
    usd_ceiling: Option<Value>,
}

/// Broker-only composition over the existing fixed Binance and Ethereum
/// adapters. Construct this only after both adapters have protected signers.
pub struct BinanceEthereumDepositAdapter {
    binance: Arc<BinanceAdapter>,
    ethereum: std::collections::BTreeMap<u64, Arc<EthereumAdapter>>,
    solana: std::collections::BTreeMap<String, Arc<gofin_solana::SolanaWalletAdapter>>,
}

impl BinanceEthereumDepositAdapter {
    pub fn new(binance: Arc<BinanceAdapter>, ethereum: Arc<EthereumAdapter>) -> Self {
        let chain_id = ethereum.config.chain_id;
        Self {
            binance,
            ethereum: std::collections::BTreeMap::from([(chain_id, ethereum)]),
            solana: std::collections::BTreeMap::new(),
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
            solana: std::collections::BTreeMap::new(),
        }
    }

    /// Add protected Solana wallets to the existing composite operation.
    pub fn with_solana_wallets(
        mut self,
        wallets: impl IntoIterator<Item = (String, Arc<gofin_solana::SolanaWalletAdapter>)>,
    ) -> Self {
        self.solana = wallets.into_iter().collect();
        self
    }

    fn input(raw: &Value) -> Result<DepositInput, GoFinError> {
        serde_json::from_value(raw.clone()).map_err(|_| {
            error(
                "INVALID_INPUT",
                "validation",
                "Binance deposit input is malformed or contains unknown fields.",
                "Use the binance.deposit schema.",
            )
        })
    }

    fn account(&self, name: &str) -> Result<ConnectionMetadata, GoFinError> {
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
    ) -> Result<(Arc<EthereumAdapter>, ConnectionMetadata), GoFinError> {
        let ethereum = self.ethereum.get(&chain_id).cloned().ok_or_else(|| {
            error(
                "EVM_NETWORK_UNAVAILABLE",
                "validation",
                "Selected EVM chain is not configured in GoFin.",
                "Choose a configured Ethereum, Arbitrum One, Robinhood Chain, or Base network.",
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

    fn solana_wallet(
        &self,
        name: &str,
    ) -> Result<Arc<gofin_solana::SolanaWalletAdapter>, GoFinError> {
        self.solana.get(name).cloned().ok_or_else(|| {
            error(
                "SOLANA_WALLET_UNKNOWN",
                "validation",
                "Selected Solana wallet is not enrolled on mainnet-beta.",
                "Select a configured protected Solana wallet.",
            )
        })
    }

    fn input_route(input: &DepositInput) -> Result<(String, String, u64), GoFinError> {
        let coin = input.coin.clone().unwrap_or_else(|| "ETH".into());
        let network = input.network.clone().unwrap_or_else(|| "ETH".into());
        if gofin_binance::binance_solana_network(&network) {
            if !matches!(coin.as_str(), "SOL" | "USDC")
                || input.chain_id.is_some()
                || input.max_fee_sol.as_deref().is_none_or(str::is_empty)
                || !input.max_fee_eth.is_empty()
            {
                return Err(error(
                    "DEPOSIT_ROUTE_UNSUPPORTED",
                    "validation",
                    "The selected Binance coin/network is not a supported Solana route.",
                    "Choose SOL or USDC on Binance's SOL network with no EVM chain ID.",
                ));
            }
            return Ok((coin, network, 0));
        }
        if input.max_fee_eth.is_empty()
            || input.max_fee_sol.is_some()
            || input.max_account_funding_sol.is_some()
        {
            return Err(error(
                "DEPOSIT_ROUTE_UNSUPPORTED",
                "validation",
                "EVM routes require maxFeeEth and do not accept Solana fee fields.",
                "Use maxFeeEth for EVM, or network SOL with maxFeeSol for Solana.",
            ));
        }
        let inferred = gofin_binance::binance_evm_chain_id(&network).unwrap_or(0);
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
                "The selected Binance coin/network is not a supported GoFin EVM route.",
                "Choose a route shown by Binance metadata on a configured GoFin EVM network.",
            ));
        }
        let expected = gofin_binance::binance_evm_chain_id(&network);
        if expected != Some(chain_id) || !gofin_networks::supports_chain(chain_id) {
            return Err(error(
                "DEPOSIT_ROUTE_UNSUPPORTED",
                "validation",
                "The Binance network does not match a supported GoFin EVM chain.",
                "Choose ETH using a current Binance network code mapped to a reviewed GoFin EVM network.",
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
    ) -> Result<(BrokerVerifiedNativeDepositTarget, AssetId), GoFinError> {
        if gofin_binance::binance_solana_network(network_name) {
            return self
                .target_solana(account_name, coin, network_name, token_request)
                .await;
        }
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

    async fn target_solana(
        &self,
        account_name: &str,
        coin: &str,
        network_name: &str,
        token_request: Option<&str>,
    ) -> Result<(BrokerVerifiedNativeDepositTarget, AssetId), GoFinError> {
        let account = self.account(account_name)?;
        let networks = self
            .binance
            .read(
                "binance.networks",
                &json!({"account":account_name,"coin":coin}),
            )
            .await?;
        let matches = networks["assets"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|row| row["coin"] == coin)
            .collect::<Vec<_>>();
        let asset_row = (matches.len() == 1).then(|| matches[0]).ok_or_else(|| {
            error(
                "DEPOSIT_NETWORK_UNAVAILABLE",
                "preview",
                "Binance did not return one unambiguous asset record.",
                "Refresh the exact SOL funding route.",
            )
        })?;
        let routes = asset_row["networks"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|row| row["network"] == network_name)
            .collect::<Vec<_>>();
        let route = (routes.len() == 1).then(|| routes[0]).ok_or_else(|| {
            error(
                "DEPOSIT_NETWORK_UNAVAILABLE",
                "preview",
                "Binance did not return one unambiguous SOL network record.",
                "Select the exact current Binance SOL network.",
            )
        })?;
        if asset_row["depositAllEnable"] != true
            || route["depositEnable"] != true
            || route["coin"] != coin
        {
            return Err(error(
                "DEPOSIT_DISABLED",
                "preview",
                "Binance does not currently accept deposits for this exact coin and network.",
                "Wait until Binance enables this route.",
            ));
        }
        let minimum = route["depositDust"].as_str().ok_or_else(|| {
            error(
                "DEPOSIT_MINIMUM_UNKNOWN",
                "preview",
                "Binance has not provided an exact minimum creditable amount.",
                "Do not send until an exact minimum is available.",
            )
        })?;
        let minimum_deposit = Quantity::from_decimal(minimum, 36).map_err(|_| internal_error())?;
        let address_data = self
            .binance
            .read(
                "binance.deposit-address",
                &json!({"account":account_name,"coin":coin,"network":network_name}),
            )
            .await?;
        let address = address_data["address"].as_str().ok_or_else(|| {
            error(
                "DEPOSIT_ADDRESS_INVALID",
                "preview",
                "Binance did not return an exact Solana deposit address.",
                "Refresh the protected deposit address.",
            )
        })?;
        gofin_solana::parse_address(address).map_err(|_| {
            error(
                "DEPOSIT_ADDRESS_INVALID",
                "preview",
                "Binance returned an invalid Solana deposit address.",
                "Do not send funds to this route.",
            )
        })?;
        if address_data["coin"] != coin
            || address_data["network"] != network_name
            || address_data["memo"]
                .as_str()
                .is_none_or(|memo| !memo.is_empty())
        {
            return Err(error(
                "DEPOSIT_ADDRESS_UNSUPPORTED",
                "preview",
                "The Binance Solana address is mismatched or requires an unsupported memo.",
                "Use a plain Solana address without a memo.",
            ));
        }
        for denomination in [&route["denomination"], &address_data["denomination"]] {
            if !denomination.is_null()
                && denomination.as_u64() != Some(1)
                && denomination.as_str() != Some("unknown")
            {
                return Err(error(
                    "DEPOSIT_DENOMINATION_UNSUPPORTED",
                    "preview",
                    "Binance reports a non-unit deposit denomination.",
                    "Do not send until the route is explicitly one-to-one.",
                ));
            }
        }
        let route_mint = route["contractAddress"]
            .as_str()
            .filter(|value| !value.is_empty() && *value != "unknown");
        let address_mint = address_data["contractAddress"]
            .as_str()
            .filter(|value| !value.is_empty() && *value != "unknown");
        let request_mint = token_request.filter(|value| !value.is_empty() && *value != "native");
        if route_mint.is_some() && address_mint.is_some() && route_mint != address_mint {
            return Err(error(
                "ASSET_IDENTITY_MISMATCH",
                "preview",
                "Binance deposit address mint differs from its network route.",
                "Refresh the exact Solana deposit route before sending.",
            ));
        }
        let metadata_mint = route_mint.or(address_mint);
        if metadata_mint.is_some() && request_mint.is_some_and(|mint| Some(mint) != metadata_mint) {
            return Err(error(
                "ASSET_IDENTITY_MISMATCH",
                "preview",
                "Requested Solana mint differs from Binance route metadata.",
                "Use the exact mint advertised by Binance.",
            ));
        }
        if let Some(address_minimum) = address_data["minimumDeposit"].as_str()
            && address_minimum != "unknown"
        {
            let address_minimum =
                Quantity::from_decimal(address_minimum, 36).map_err(|_| internal_error())?;
            if address_minimum != minimum_deposit {
                return Err(error(
                    "DEPOSIT_MINIMUM_MISMATCH",
                    "preview",
                    "Binance network and deposit-address minimums disagree.",
                    "Refresh the exact deposit route before sending.",
                ));
            }
        }
        let source_asset = gofin_solana::funding::resolve_external_asset(
            &self
                .solana
                .values()
                .next()
                .ok_or_else(|| {
                    error(
                        "SOLANA_WALLET_UNKNOWN",
                        "preview",
                        "No protected Solana wallet observer is configured.",
                        "Select a configured Solana wallet.",
                    )
                })?
                .runtime(),
            coin,
            metadata_mint.or(request_mint),
        )
        .await?;
        if (coin == "SOL" && source_asset.reference != "native")
            || (coin == "USDC" && source_asset.reference == "native")
        {
            return Err(error(
                "ASSET_IDENTITY_MISMATCH",
                "preview",
                "Binance route identity does not match the selected Solana asset.",
                "Use native SOL or the exact Solana USDC mint.",
            ));
        }
        let evidence = json!({"accountResource":account.resource,"coin":coin,"network":network_name,"address":address,"mint":source_asset.reference,"denomination":1,"minimumDeposit":minimum_deposit,"depositEnabled":true,"memo":"","minimumSource":"Binance capital config depositDust"});
        let evidence_digest = binding_hash(&evidence).map_err(|_| internal_error())?;
        let observed_at = now();
        let expires_at = chrono::DateTime::parse_from_rfc3339(&observed_at)
            .map_err(|_| internal_error())?
            .with_timezone(&chrono::Utc)
            .checked_add_signed(chrono::Duration::seconds(90))
            .ok_or_else(internal_error)?
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        Ok((
            BrokerVerifiedNativeDepositTarget {
                chain_id: 0,
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

    fn solana_inner_input(
        input: &DepositInput,
        target: &BrokerVerifiedNativeDepositTarget,
        asset: &AssetId,
        wallet: &gofin_solana::SolanaWalletAdapter,
    ) -> Result<Value, GoFinError> {
        let max_fee_sol = input
            .max_fee_sol
            .as_deref()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                error(
                    "FEE_BOUND_REQUIRED",
                    "preview",
                    "Solana deposits require an explicit maximum network fee in SOL.",
                    "Set maxFeeSol to the fee bound you approve.",
                )
            })?;
        let mut inner = json!({"wallet":input.wallet,"network":"solana","cluster":"mainnet-beta","recipient":target.address,"amount":input.amount,"max_fee_sol":max_fee_sol,"max_account_funding_sol":input.max_account_funding_sol,"requestId":input.request_id});
        if asset.reference != "native" {
            inner["mint"] = Value::String(asset.reference.clone());
            let token_account = gofin_solana::associated_token_address(
                wallet.address(),
                &asset.reference,
                gofin_solana::token::TOKEN_PROGRAM_ID,
            )
            .map_err(|_| internal_error())?;
            inner["source_token_account"] = Value::String(token_account);
        }
        Ok(inner)
    }

    async fn source_free(&self, account: &str, coin: &str) -> Result<String, GoFinError> {
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
    ) -> Result<FinancialIntent, GoFinError> {
        let source = inner
            .intent
            .debits
            .first()
            .map(|debit| debit.resource.clone())
            .ok_or_else(|| {
                error(
                    "INNER_PREVIEW_INVALID",
                    "preview",
                    "Wallet transfer preview omitted its source resource.",
                    "Refresh the selected wallet transfer preview.",
                )
            })?;
        let debit_atoms = Quantity::from_decimal(amount, source_asset.decimals).map_err(|_| {
            error(
                "INVALID_AMOUNT",
                "preview",
                "Amount exceeds the selected asset's exact on-chain precision.",
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
        let mut locks = inner.intent.locks.clone();
        locks.push(source.clone());
        locks.push(target.account_resource.clone());
        locks.sort();
        locks.dedup();
        let mut expected = inner.intent.expected.clone();
        let principal = expected.iter_mut().find(|effect| matches!(effect,
            EffectConstraint::Transfer { from, debit_asset, .. } if from == &source && debit_asset == source_asset
        )).ok_or_else(|| error("INNER_PREVIEW_INVALID", "preview", "Wallet preview omitted its principal transfer constraint.", "Refresh the wallet transfer preview."))?;
        *principal = EffectConstraint::Transfer {
            from: source.clone(),
            to: target.account_resource.clone(),
            debit_asset: source_asset.clone(),
            credit_asset: target.asset.clone(),
            maximum_debit: debit_atoms,
            minimum_credit: credit_atoms,
        };
        Ok(FinancialIntent {
            debits: inner.intent.debits.clone(),
            maximum_costs: inner.intent.maximum_costs.clone(),
            expected,
            locks,
        })
    }

    pub fn operation_definition() -> Definition {
        let mut input_schema =
            serde_json::to_value(schema_for!(DepositInput)).unwrap_or(Value::Null);
        if let Some(required) = input_schema
            .get_mut("required")
            .and_then(Value::as_array_mut)
        {
            required.retain(|field| !matches!(field.as_str(), Some("maxFeeEth" | "maxFeeSol")));
        }
        input_schema["oneOf"] = json!([
            {"required":["maxFeeEth"],"not":{"anyOf":[{"required":["network"]},{"required":["chainId"]},{"required":["maxFeeSol"]}] }},
            {"required":["network","maxFeeEth"],"properties":{"network":{"not":{"const":"SOL"}}},"not":{"required":["maxFeeSol"]}},
            {"required":["network","coin","maxFeeSol"],"properties":{"network":{"const":"SOL"},"coin":{"enum":["SOL","USDC"]}},"not":{"anyOf":[{"required":["maxFeeEth"]},{"required":["chainId"]}]}}
        ]);
        Definition {
            name: "binance.deposit".into(),
            description: "Deposit native SOL or ETH, or exact Solana SPL USDC / EVM ERC20 assets, from a protected wallet into a Binance main account or subaccount using its own credentials. Live Binance metadata selects the asset/network route; completion requires finalized transaction evidence and matching usable Binance credit.".into(),
            kind: OperationKind::Write,
            execution_actor: ExecutionActor::AgentInvocable,
            input_schema,
            output_schema: json!({"type":"object"}),
            error_schema: schema_for!(GoFinError).into(),
            examples: vec![
                json!({"requestId":"deposit-native-001","account":"binance-sub","wallet":"wallet","coin":"ETH","network":"ARBITRUM","chainId":42161,"token":"native","amount":"0.001","maxFeeEth":"0.0001"}),
                json!({"requestId":"deposit-token-001","account":"binance-sub","wallet":"wallet","coin":"USDC","network":"ARBITRUM","chainId":42161,"token":"0xaf88d065e77c8cc2239327c5edb3a432268e5831","amount":"1","maxFeeEth":"0.0001"}),
                json!({"requestId":"deposit-sol-001","account":"binance-sub","wallet":"solana-wallet","coin":"SOL","network":"SOL","amount":"0.01","maxFeeSol":"0.00001"}),
                json!({"requestId":"deposit-sol-usdc-001","account":"binance-sub","wallet":"solana-wallet","coin":"USDC","network":"SOL","token":"EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v","amount":"1","maxFeeSol":"0.00001"}),
            ],
            coverage: "Synthetic broker-composite transport and finalized fake-chain proof for configured EVM and Solana wallets; live funding capabilities and acceptance remain separately reported.".into(),
            completion: "Original transaction identity is canonical and finalized, and exactly one usable matching Binance credit plus the corresponding free-balance increase is observed.".into(),
            semantics: json!({"legacyDefaults":"Omitted coin/network select ETH on Ethereum mainnet; omitted chainId resolves the reviewed Binance EVM network mapping. Solana routes use coin SOL or USDC with network SOL and no chainId.","accounts":"Main or subaccount using its own credential; no master required. Parent-authorized hierarchy reads remain optional.","sourceCoverage":"Configured GoFin EVM chains and Solana mainnet-beta wallets. Solana token transfers use an explicit mint or the pinned mainnet USDC mint; other assets, memo/tag routes and non-unit denominations are unavailable.","approval":"Approval binds source wallet, destination account, chain/network, exact asset, deposit address, amount, fee envelope and displayed USD ceilings.","recovery":"Reconcile the original operation after response loss, delayed credit or restart; do not resend under a new request ID.","recipient":"fixed from the authenticated account's current Binance deposit-address endpoint","token":"native SOL/ETH or exact SPL USDC/ERC20 identity; mint/contract and decimals are checked against current metadata and chain state","amount":"exact finite amount; amount=all is unsupported","completion":"Transaction submission alone is insufficient; the exact finalized transaction must match one Binance deposit-history credit and the exact free-balance delta. Status 6 may show a withdrawal hold and settles only when the credit is reflected in free balance.","venueAssetDecimals":36,"maxFeeEth":"Maximum total chain-native fee in ETH for EVM routes.","maxFeeSol":"Maximum total transaction fee in SOL for Solana routes; any recipient account funding is separately bounded.","broker_generated_attempt_input_fields":["/requestId"]}),
        }
    }

    async fn prepare_deposit(
        &self,
        operation: &str,
        raw: &Value,
        approved_inner: Option<&PreparedOperation>,
    ) -> Result<PreparedOperation, GoFinError> {
        let input = Self::input(raw)?;
        let account = self.account(&input.account)?;
        let (coin, network, chain_id) = Self::input_route(&input)?;
        if chain_id == 0 {
            return self
                .prepare_solana_deposit(operation, raw, &input, &coin, &network, approved_inner)
                .await;
        }
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
            source_protocol: None,
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
            warnings: vec!["A successful Ethereum transaction does not complete this operation until Binance records the exact deposit credit and matching free-balance increase.".into()],
            observed_at: inner_preview.observed_at.clone(),
            expires_at: inner_preview.expires_at.clone(),
            valuation: inner_preview.valuation.clone(),
            intent,
            operation_version: 2,
            action_deadline: None,
        };
        // Compose the wallet's approved USD ceilings without changing its exact
        // principal or gas envelope. Legacy inner previews retain exact pricing.
        if approved_inner.is_none()
            || inner_preview
                .binding
                .product_get(gofin_domain::value_bounds::KEY)?
                .is_some()
        {
            gofin_domain::value_bounds::bind(&mut prepared);
        }
        Ok(prepared)
    }

    async fn prepare_solana_deposit(
        &self,
        operation: &str,
        raw: &Value,
        input: &DepositInput,
        coin: &str,
        network: &str,
        approved_inner: Option<&PreparedOperation>,
    ) -> Result<PreparedOperation, GoFinError> {
        if input.request_id.is_empty() || input.request_id.len() > 128 {
            return Err(error(
                "DEPOSIT_CONFIGURATION_INVALID",
                "preview",
                "Deposit requires a bounded stable request ID.",
                "Use a new request ID for this transfer.",
            ));
        }
        let wallet = self.solana_wallet(&input.wallet)?;
        if wallet.signer().is_none() {
            return Err(error(
                "PROTECTED_SIGNER_REQUIRED",
                "preview",
                "Selected Solana wallet is not signing-enabled.",
                "Choose a protected signing wallet.",
            ));
        }
        let (target, source_asset) = self
            .target(
                &input.account,
                &input.wallet,
                coin,
                network,
                0,
                input.token.as_deref(),
            )
            .await?;
        let amount_atoms =
            Quantity::from_decimal(&input.amount, source_asset.decimals).map_err(|_| {
                error(
                    "INVALID_AMOUNT",
                    "preview",
                    "Deposit amount must be exact within the selected Solana asset precision.",
                    "Use a finite exact SOL or USDC amount.",
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
        let venue_atoms =
            Quantity::from_decimal(&input.amount, 36).map_err(|_| internal_error())?;
        if venue_atoms < target.minimum_deposit {
            return Err(error(
                "DEPOSIT_MINIMUM",
                "preview",
                "Deposit amount is below Binance's observed creditable minimum.",
                "Increase the amount or choose another route.",
            ));
        }
        let source_free_before = self.source_free(&input.account, coin).await?;
        let inner_input = Self::solana_inner_input(input, &target, &source_asset, &wallet)?;
        let inner_preview = match approved_inner {
            Some(original) => wallet.revalidate(original).await?,
            None => wallet.prepare("wallet.transfer", &inner_input).await?,
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
                "Solana transfer preview did not preserve the exact selected mint.",
                "Refresh the Binance route and exact mint metadata.",
            ));
        }
        let intent =
            Self::cross_custody_intent(&inner_preview, &target, &input.amount, &source_asset)?;
        let summary = format!(
            "Send {} {} from Solana wallet {} to Binance {} ({}) on SOL; maximum network fee {} SOL.",
            input.amount,
            coin,
            input.wallet,
            input.account,
            target.address,
            input.max_fee_sol.as_deref().unwrap_or("")
        );
        let binding = DepositBinding {
            target,
            inner_input,
            inner_preview: inner_preview.clone(),
            source_free_before,
            source_protocol: Some("solana".into()),
            coin: coin.into(),
            source_asset: Some(source_asset.clone()),
            usd_ceiling: None,
        };
        let mut prepared = PreparedOperation {
            operation: operation.into(), input: raw.clone(), binding: serde_json::to_value(binding).map_err(|_| internal_error())?,
            summary,
            checks: vec![Check { name:"venue_deposit_enabled".into(), status:CheckStatus::Passed, detail:format!("Binance confirms {coin} deposits are enabled on SOL and supplies an exact credit minimum.") }, Check { name:"asset_identity".into(), status:CheckStatus::Passed, detail:format!("Exact Solana asset {} with {} decimals is fixed by finalized mint metadata.", source_asset.reference, source_asset.decimals) }, Check { name:"solana_transfer_preview".into(), status:CheckStatus::Passed, detail:"Ordinary wallet.transfer froze the exact owner, recipient, amount, mint and fee limits.".into() }],
            warnings: vec!["A finalized Solana transaction does not complete this operation until Binance records the exact usable deposit credit and matching free-balance increase.".into()],
            observed_at:inner_preview.observed_at.clone(), expires_at:inner_preview.expires_at.clone(), valuation:inner_preview.valuation.clone(), intent, operation_version:2, action_deadline:None,
        };
        if approved_inner.is_none()
            || inner_preview
                .binding
                .product_get(gofin_domain::value_bounds::KEY)?
                .is_some()
        {
            gofin_domain::value_bounds::bind(&mut prepared);
        }
        Ok(prepared)
    }
}

fn internal_error() -> GoFinError {
    error(
        "INTERNAL_ERROR",
        "broker",
        "Protected deposit record is invalid.",
        "Inspect the protected broker diagnostics.",
    )
}

#[async_trait]
impl Adapter for BinanceEthereumDepositAdapter {
    fn uses_rpc_reads(&self, operation: &str, input: &Value) -> bool {
        operation == "binance.deposit"
            && Self::input(input)
                .and_then(|input| Self::input_route(&input))
                .is_ok_and(|(_, _, chain_id)| chain_id != 0)
    }
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
        // every constituent wallet identity without leaking internal
        // destination rows into `gofin connections list`.
        let mut identities = self.binance.authorization_identities();
        for adapter in self.ethereum.values() {
            for identity in adapter.authorization_identities() {
                if !identities.contains(&identity) {
                    identities.push(identity);
                }
            }
        }
        for adapter in self.solana.values() {
            for identity in adapter.authorization_identities() {
                if !identities.contains(&identity) {
                    identities.push(identity);
                }
            }
        }
        identities
    }

    fn snapshot_identities(&self) -> Vec<ConnectionMetadata> {
        // Constituent account/wallet observers own financial coverage.
        Vec::new()
    }

    async fn snapshot(&self) -> Result<StateSnapshot, GoFinError> {
        Ok(StateSnapshot {
            state: FinancialState::default(),
            observed_at: now(),
            complete: true,
            coverage: Vec::new(),
            pending: Vec::new(),
        })
    }

    async fn read(&self, _operation: &str, _input: &Value) -> Result<Value, GoFinError> {
        Err(error(
            "WRONG_OPERATION_KIND",
            "read",
            "Deposit is a write operation.",
            "Use preview before an authorized execute.",
        ))
    }

    async fn prepare(
        &self,
        operation: &str,
        input: &Value,
    ) -> Result<PreparedOperation, GoFinError> {
        if operation != "binance.deposit" {
            return Err(error(
                "UNSUPPORTED_OPERATION",
                "preview",
                "Composite supports metadata-verified Binance deposits over configured EVM and Solana networks.",
                "Select native SOL/ETH or exact USDC on a supported network.",
            ));
        }
        self.prepare_deposit(operation, input, None).await
    }

    async fn prepare_with_context(
        &self,
        operation: &str,
        input: &Value,
        _context: &gofin_domain::PreparationContext,
    ) -> Result<PreparedOperation, GoFinError> {
        self.prepare(operation, input).await
    }

    async fn preflight_return_route(
        &self,
        operation: &str,
        raw: &Value,
        context: &ReturnRouteContext,
    ) -> Result<ReturnRouteProof, GoFinError> {
        if operation != "binance.deposit" {
            return Err(error(
                "UNSUPPORTED_OPERATION",
                "preflight",
                "Composite supports metadata-verified Binance deposits over configured EVM and Solana networks.",
                "Select native SOL/ETH or exact USDC on a supported network.",
            ));
        }
        let input = Self::input(raw)?;
        let (coin, network, chain_id) = Self::input_route(&input)?;
        if chain_id == 0 {
            let wallet = self.solana_wallet(&input.wallet)?;
            let (target, source_asset) = self
                .target(
                    &input.account,
                    &input.wallet,
                    &coin,
                    &network,
                    0,
                    input.token.as_deref(),
                )
                .await?;
            let inner_input = Self::solana_inner_input(&input, &target, &source_asset, &wallet)?;
            let mut route_context = context.clone();
            if !route_context
                .projected
                .cash
                .iter()
                .any(|cash| cash.resource == target.account_resource && cash.asset == target.asset)
            {
                let free = self.source_free(&input.account, &coin).await?;
                let exact = Quantity::from_decimal(&free, 36).map_err(|_| internal_error())?;
                route_context
                    .projected
                    .cash
                    .push(gofin_domain::ProjectedCash {
                        resource: target.account_resource.clone(),
                        asset: target.asset.clone(),
                        amount: gofin_domain::QuantityInterval {
                            minimum: exact.clone(),
                            maximum: exact,
                        },
                    });
            }
            let mut proof = wallet
                .preflight_return_route("wallet.transfer", &inner_input, &route_context)
                .await?;
            proof.operation = operation.into();
            proof.input_hash = binding_hash(raw)?;
            proof.evidence["composite"] = json!({"operation":operation,"account":input.account,"wallet":input.wallet,"targetEvidenceDigest":target.evidence_digest,"completion":"finalized Solana transaction plus exact matching usable Binance credit required"});
            return Ok(proof);
        }
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
                .push(gofin_domain::ProjectedCash {
                    resource: target.account_resource.clone(),
                    asset: target.asset.clone(),
                    amount: gofin_domain::QuantityInterval {
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
    ) -> Result<PreparedOperation, GoFinError> {
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
        let target_address_changed = if original.source_protocol.as_deref() == Some("solana") {
            original.target.address != current.target.address
        } else {
            !original
                .target
                .address
                .eq_ignore_ascii_case(&current.target.address)
        };
        if original.source_protocol != current.source_protocol
            || original.target.chain_id != current.target.chain_id
            || original.target.account_resource != current.target.account_resource
            || target_address_changed
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
                "Binance target, asset identity, source balance, or wallet transfer binding changed.",
                "Obtain a fresh preview and approval.",
            ));
        }
        let mut fresh = fresh;
        gofin_domain::value_bounds::carry(prepared, &mut fresh)?;
        // Keep the approved immutable target evidence in the returned binding;
        // fresh timestamp fields are used for expiry only and never replace it.
        fresh.binding = prepared.binding.clone();
        Ok(fresh)
    }

    async fn submit(
        &self,
        prepared: &PreparedOperation,
        journal: Arc<dyn gofin_domain::SubmissionJournal>,
    ) -> Result<Submission, GoFinError> {
        let fresh = self.revalidate(prepared).await?;
        let binding: DepositBinding =
            serde_json::from_value(fresh.binding.clone()).map_err(|_| internal_error())?;
        if binding.source_protocol.as_deref() == Some("solana") {
            let input = Self::input(&fresh.input)?;
            let wallet = self.solana_wallet(&input.wallet)?;
            let validated = gofin_domain::RevalidatedOperation::after_revalidation(
                fresh,
                Arc::new(gofin_domain::clock::SystemClock),
            )?;
            return wallet
                .submit_nested_revalidated(&validated, &binding.inner_preview, journal)
                .await;
        }
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

    async fn submit_revalidated(
        &self,
        validated: &gofin_domain::RevalidatedOperation,
        journal: Arc<dyn gofin_domain::SubmissionJournal>,
    ) -> Result<Submission, GoFinError> {
        validated.assert_fresh()?;
        // The engine has already performed the final adapter revalidation and
        // bound this complete candidate to policy and the durable journal.
        // Repeating Binance and Solana RPC reads here would add latency after
        // approval without strengthening the child-message binding.
        let candidate = validated.prepared();
        let binding: DepositBinding =
            serde_json::from_value(candidate.binding.clone()).map_err(|_| internal_error())?;
        if binding.source_protocol.as_deref() == Some("solana") {
            let input = Self::input(&candidate.input)?;
            let wallet = self.solana_wallet(&input.wallet)?;
            let child_amount = binding.inner_input.get("amount").and_then(Value::as_str);
            let child_recipient = binding.inner_input.get("recipient").and_then(Value::as_str);
            let child_wallet = binding.inner_input.get("wallet").and_then(Value::as_str);
            if child_amount != Some(input.amount.as_str())
                || child_recipient != Some(binding.target.address.as_str())
                || child_wallet != Some(input.wallet.as_str())
                || binding.source_asset.as_ref().is_none_or(|asset| {
                    asset.network != "solana:mainnet-beta"
                        || (input.coin.as_deref() == Some("SOL") && asset.reference != "native")
                        || (input.coin.as_deref() == Some("USDC")
                            && asset.reference != gofin_solana::funding::MAINNET_USDC_MINT)
                })
            {
                return Err(error(
                    "PREVIEW_CHANGED",
                    "submission",
                    "The nested Solana transfer no longer matches the approved Binance deposit.",
                    "Obtain a fresh preview and approval.",
                ));
            }
            // The outer candidate remains the journal identity; the nested
            // transfer is only its exact frozen transaction payload.
            return wallet
                .submit_nested_revalidated(validated, &binding.inner_preview, journal)
                .await;
        }
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

    async fn reconcile(&self, record: &OperationRecord) -> Result<Reconciliation, GoFinError> {
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
        let chain = if binding.source_protocol.as_deref() == Some("solana") {
            let input = Self::input(&record.input)?;
            let wallet = match self.solana_wallet(&input.wallet) {
                Ok(wallet) => wallet,
                Err(err) => return Ok(unknown(json!({"reason":err.code,"source":"solana"}))),
            };
            wallet.reconcile(&inner_record).await?
        } else {
            let Some(ethereum) = self.ethereum.get(&binding.target.chain_id) else {
                return Ok(unknown(
                    json!({"reason":"The original EVM network is no longer configured; the durable submission was not replayed.","chainId":binding.target.chain_id}),
                ));
            };
            ethereum.reconcile(&inner_record).await?
        };
        if chain.status != Status::Completed {
            return Ok(Reconciliation {
                status: chain.status,
                evidence: json!({"chain":chain.evidence,"venueCredit":"not yet proved"}),
                verified: chain.verified,
            });
        }
        let Some(mut verified) = chain.verified else {
            return Ok(unknown(
                json!({"reason":"Canonical source-chain completion did not include verified financial effects."}),
            ));
        };
        if verified.submission != submission {
            return Ok(unknown(
                json!({"reason":"Source-chain proof submission differs from the original persisted identity."}),
            ));
        }
        let is_solana = binding.source_protocol.as_deref() == Some("solana");
        let tx_hash = if is_solana {
            submission
                .data
                .get("wallet_signature")
                .and_then(Value::as_str)
                .filter(|signature| !signature.is_empty())
                .unwrap_or(&submission.reference)
                .to_owned()
        } else {
            submission.reference.clone()
        };
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
                &json!({"account":input.account,"coin":coin,"network":binding.target.network,"txHash":tx_hash}),
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
        let address_matches = row["address"].as_str().is_some_and(|address| {
            if is_solana {
                address == binding.target.address
            } else {
                address.eq_ignore_ascii_case(&binding.target.address)
            }
        });
        let tx_matches = row["txId"].as_str().is_some_and(|hash| {
            if is_solana {
                hash == tx_hash
            } else {
                hash.eq_ignore_ascii_case(&tx_hash)
            }
        });
        if row["coin"] != coin
            || row["network"] != binding.target.network
            || !address_matches
            || !tx_matches
            || !(row["addressTag"].is_null()
                || row["addressTag"].as_str().is_some_and(str::is_empty))
        {
            return Ok(unknown(
                json!({"chain":chain.evidence,"venueRow":row,"reason":"Deposit row is not one exact usable credit for the original transaction and target."}),
            ));
        }
        // Status 6 denotes a credited deposit that may carry a withdrawal
        // hold. As on the EVM path, it settles only with the exact free-balance
        // delta proved below; a locked credit alone remains pending.
        let usable_status = matches!(row["status"].as_u64(), Some(1 | 6));
        if row["credited"] != true || !usable_status {
            return Ok(unknown(
                json!({"chain":chain.evidence,"venueRow":row,"reason":"Deposit row does not confirm usable credit with clear travel-rule status for the original transaction and target."}),
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
        let expected_destination = if is_solana {
            ResourceId(format!("solana:mainnet-beta:{}", binding.target.address))
        } else {
            ResourceId(format!(
                "ethereum:{}:{}",
                binding.target.chain_id,
                binding.target.address.to_ascii_lowercase()
            ))
        };
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
                } if !transfer_seen
                    && to == expected_destination
                    && debit.asset == expected_asset =>
                {
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
                            json!({"chain":chain.evidence,"venueRow":row,"reason":"Venue credited quantity does not exactly match the original wallet asset transfer principal."}),
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
                FinancialEffect::Transfer { .. } => effects.push(effect),
                other => {
                    return Ok(unknown(
                        json!({"chain":chain.evidence,"reason":"Unexpected extra chain effect is not safe to translate into venue custody.","effect":other}),
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
        verified.evidence = json!({"method":if is_solana {"solana-finalized-and-binance-usable-deposit-v1"} else {"evm-canonical-and-binance-usable-deposit-v2"},"withdrawal_locked":row["withdrawalLocked"] == true,"chain":chain.evidence,"venueHistory":history,"venueBalance":{"before":binding.source_free_before,"after":after,"delta":credit},"targetEvidenceDigest":binding.target.evidence_digest,"coin":coin,"network":binding.target.network,"asset":expected_asset,"originalSubmission":submission});
        verified.observed_at = now();
        verified.proof_digest = verified_effect_digest(&verified)?;
        Ok(Reconciliation {
            status: Status::Completed,
            evidence: verified.evidence.clone(),
            verified: Some(verified),
        })
    }
}

#[cfg(test)]
mod route_tests {
    use super::*;

    #[test]
    fn base_deposit_network_matches_exact_reviewed_chain_and_keeps_legacy_defaults() {
        let input: DepositInput = serde_json::from_value(json!({
            "requestId":"synthetic-base-deposit", "account":"account", "wallet":"wallet",
            "coin":"ANYCOIN", "network":"BASE", "token":"0x1111111111111111111111111111111111111111",
            "amount":"1", "maxFeeEth":"0.001"
        })).unwrap();
        assert_eq!(
            BinanceEthereumDepositAdapter::input_route(&input).unwrap(),
            ("ANYCOIN".into(), "BASE".into(), 8453)
        );
        let mut mismatched = input;
        mismatched.chain_id = Some(1);
        assert_eq!(
            BinanceEthereumDepositAdapter::input_route(&mismatched)
                .unwrap_err()
                .code,
            "DEPOSIT_ROUTE_UNSUPPORTED"
        );
        let legacy: DepositInput = serde_json::from_value(json!({
            "requestId":"synthetic-legacy-deposit", "account":"account", "wallet":"wallet",
            "amount":"1", "maxFeeEth":"0.001"
        }))
        .unwrap();
        assert_eq!(
            BinanceEthereumDepositAdapter::input_route(&legacy).unwrap(),
            ("ETH".into(), "ETH".into(), 1)
        );
    }

    #[test]
    fn solana_deposit_routes_require_sol_fee_and_reject_evm_identity() {
        let input: DepositInput = serde_json::from_value(json!({
            "requestId":"solana-deposit", "account":"account", "wallet":"wallet",
            "coin":"SOL", "network":"SOL", "amount":"0.01", "maxFeeSol":"0.00001"
        }))
        .unwrap();
        assert_eq!(
            BinanceEthereumDepositAdapter::input_route(&input).unwrap(),
            ("SOL".into(), "SOL".into(), 0)
        );

        let mut missing_fee = input.clone();
        missing_fee.max_fee_sol = None;
        assert_eq!(
            BinanceEthereumDepositAdapter::input_route(&missing_fee)
                .unwrap_err()
                .code,
            "DEPOSIT_ROUTE_UNSUPPORTED"
        );
        let mut wrong_chain = input.clone();
        wrong_chain.chain_id = Some(1);
        assert_eq!(
            BinanceEthereumDepositAdapter::input_route(&wrong_chain)
                .unwrap_err()
                .code,
            "DEPOSIT_ROUTE_UNSUPPORTED"
        );
        let mut zero_chain = input.clone();
        zero_chain.chain_id = Some(0);
        assert_eq!(
            BinanceEthereumDepositAdapter::input_route(&zero_chain)
                .unwrap_err()
                .code,
            "DEPOSIT_ROUTE_UNSUPPORTED"
        );
        let mut conflicting_fee = input.clone();
        conflicting_fee.max_fee_eth = "0.001".into();
        assert_eq!(
            BinanceEthereumDepositAdapter::input_route(&conflicting_fee)
                .unwrap_err()
                .code,
            "DEPOSIT_ROUTE_UNSUPPORTED"
        );

        let evm_without_fee: DepositInput = serde_json::from_value(json!({
            "requestId":"evm-deposit", "account":"account", "wallet":"wallet",
            "amount":"1"
        }))
        .unwrap();
        assert_eq!(
            BinanceEthereumDepositAdapter::input_route(&evm_without_fee)
                .unwrap_err()
                .code,
            "DEPOSIT_ROUTE_UNSUPPORTED"
        );
    }

    #[test]
    fn composite_intent_retains_solana_associated_account_funding() {
        let source = ResourceId("solana:mainnet-beta:source".into());
        let target_resource = ResourceId("binance:uid:4:SPOT".into());
        let usdc = AssetId {
            network: "solana:mainnet-beta".into(),
            reference: "mint".into(),
            decimals: 6,
        };
        let sol = AssetId {
            network: "solana:mainnet-beta".into(),
            reference: "native".into(),
            decimals: 9,
        };
        let amount = Quantity::from_u64(1_000_000);
        let rent = Quantity::from_u64(3_000_000);
        let fee = Quantity::from_u64(10_000);
        let inner = PreparedOperation {
            operation: "wallet.transfer".into(),
            input: Value::Null,
            binding: Value::Null,
            summary: String::new(),
            checks: vec![],
            warnings: vec![],
            observed_at: now(),
            expires_at: now(),
            valuation: None,
            intent: FinancialIntent {
                debits: vec![
                    gofin_domain::ResourceAmount {
                        resource: source.clone(),
                        amount: Amount {
                            asset: usdc.clone(),
                            atoms: amount.clone(),
                        },
                    },
                    gofin_domain::ResourceAmount {
                        resource: source.clone(),
                        amount: Amount {
                            asset: sol.clone(),
                            atoms: rent.clone(),
                        },
                    },
                ],
                maximum_costs: vec![gofin_domain::CostBound {
                    location: gofin_domain::AccountingLocation::Cash {
                        resource: source.clone(),
                    },
                    amount: Amount {
                        asset: sol.clone(),
                        atoms: fee.clone(),
                    },
                    treatment: gofin_domain::CostTreatment::AdditionalDebit,
                }],
                expected: vec![
                    EffectConstraint::Transfer {
                        from: source.clone(),
                        to: ResourceId("solana:mainnet-beta:binance-owner".into()),
                        debit_asset: usdc.clone(),
                        credit_asset: usdc.clone(),
                        maximum_debit: amount.clone(),
                        minimum_credit: amount.clone(),
                    },
                    EffectConstraint::Transfer {
                        from: source.clone(),
                        to: ResourceId("solana:mainnet-beta:account:binance-owner".into()),
                        debit_asset: sol.clone(),
                        credit_asset: sol.clone(),
                        maximum_debit: rent.clone(),
                        minimum_credit: rent.clone(),
                    },
                ],
                locks: vec![source.clone()],
            },
            operation_version: 2,
            action_deadline: None,
        };
        let target = BrokerVerifiedNativeDepositTarget {
            chain_id: 0,
            account_resource: target_resource.clone(),
            address: "binance-owner".into(),
            asset: AssetId {
                network: "binance".into(),
                reference: "USDC".into(),
                decimals: 36,
            },
            minimum_deposit: Quantity::from_u64(0),
            network: "SOL".into(),
            observed_at: now(),
            expires_at: now(),
            evidence_digest: "digest".into(),
        };
        let combined =
            BinanceEthereumDepositAdapter::cross_custody_intent(&inner, &target, "1", &usdc)
                .unwrap();
        assert_eq!(combined.debits.len(), 2);
        assert_eq!(combined.expected.len(), 2);
        assert_eq!(combined.maximum_costs, inner.intent.maximum_costs);
        assert_eq!(
            combined.expected[0],
            EffectConstraint::Transfer {
                from: source,
                to: target_resource,
                debit_asset: usdc.clone(),
                credit_asset: AssetId {
                    network: "binance".into(),
                    reference: "USDC".into(),
                    decimals: 36
                },
                maximum_debit: amount.clone(),
                minimum_credit: Quantity::from_decimal("1", 36).unwrap(),
            }
        );
        assert_eq!(combined.expected[1], inner.intent.expected[1]);
    }
}
