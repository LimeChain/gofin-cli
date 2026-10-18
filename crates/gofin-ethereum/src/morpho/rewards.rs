//! Merkl wallet/token aggregate claims. API data never grants signing authority.
use crate::*;
use gofin_domain::{
    boundaries::BoundarySpec,
    financial_plans::{FinancialPlanFundingIdentity, PlanPermission},
};
use gofin_evm::alloy::{
    primitives::{B256, keccak256},
    sol,
    sol_types::{SolCall, SolEvent, SolValue},
};
use gofin_evm::{CanonicalReceipt, block_tag, rpc_bytes};

sol! {
    interface MerklDistributor {
        function getMerkleRoot() external view returns (bytes32);
        function claimed(address user, address token) external view returns (uint208 amount, uint48 timestamp, bytes32 merkleRoot);
        function claimRecipient(address user, address token) external view returns (address);
        function claim(address[] users, address[] tokens, uint256[] amounts, bytes32[][] proofs) external;
        event Claimed(address indexed user, address indexed token, uint256 amount);
    }
}

pub const DISTRIBUTOR: &str = "0x3ef3d8ba38ebe18db133cec108f4d14ce00dd9ae";
const IMPLEMENTATION_SLOT: &str =
    "0x360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc";
const CLAIM_OPERATION: &str = "morpho.rewards.claim";
const AGGREGATE_NOTICE: &str = "Claims include the entire selected wallet/token Merkl aggregate, including rewards from other protocols and campaigns. Pending rewards are not claimable.";
const PAYOUT_NOTICE: &str = "The displayed payout is an estimate with no guaranteed minimum: another authorized claim before execution can reduce it, including to zero. Only this transaction's verified incremental payment is credited.";

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RewardsReadInput {
    pub network: String,
    pub chain_id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wallet: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vault: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<Vec<String>>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RewardClaimInput {
    pub wallet: String,
    pub network: String,
    pub chain_id: u64,
    pub tokens: Vec<String>,
    pub request_id: String,
    pub max_fee_eth: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RewardClaimBounds {
    /// Exact token address -> maximum incremental payout in token units.
    pub max_amounts: BTreeMap<String, String>,
    pub max_fee_eth: String,
}

#[derive(Clone, Debug, Deserialize)]
struct ApiChain {
    id: u64,
}
#[derive(Clone, Debug, Deserialize)]
struct ApiToken {
    address: String,
    decimals: u8,
}
#[derive(Clone, Debug, Deserialize)]
struct ApiReward {
    token: ApiToken,
    amount: String,
    claimed: String,
    pending: String,
    proofs: Vec<String>,
    #[serde(default)]
    breakdowns: Vec<Value>,
}
#[derive(Clone, Debug, Deserialize)]
struct ApiRewards {
    chain: ApiChain,
    rewards: Vec<ApiReward>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct ClaimToken {
    token: AssetId,
    cumulative: Quantity,
    previously_claimed: Quantity,
    payout: Quantity,
    proofs: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ClaimPayoutSemantics {
    CumulativeRemainderAtExecutionNoGuaranteedMinimum,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ClaimBinding {
    version: u32,
    transaction: FrozenTransaction,
    resource: ResourceId,
    distributor: String,
    root: String,
    claims: Vec<ClaimToken>,
    // Absent only in the original exact-payout binding. Do not silently weaken
    // an already-authorized v1 intent when reconciling stored operations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    payout_semantics: Option<ClaimPayoutSemantics>,
    #[serde(
        default,
        rename = "_gofin_usd_ceiling_v1",
        alias = "_fin_usd_ceiling_v1",
        skip_serializing_if = "Option::is_none"
    )]
    value_bounds: Option<Value>,
    #[serde(
        default,
        rename = "__gofin_boundary_authorization",
        alias = "__fin_boundary_authorization",
        skip_serializing_if = "Option::is_none"
    )]
    boundary_authorization: Option<Value>,
    #[serde(
        default,
        rename = "__gofin_financial_plan",
        alias = "__fin_financial_plan",
        skip_serializing_if = "Option::is_none"
    )]
    financial_plan: Option<Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct RewardRateAsset {
    address: String,
    symbol: String,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct RewardRate {
    supply_apr: serde_json::Number,
    asset: RewardRateAsset,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct VaultRewards {
    address: String,
    apy: Option<serde_json::Number>,
    net_apy: Option<serde_json::Number>,
    rewards: Vec<RewardRate>,
}

fn parse_claim(input: &Value) -> Result<RewardClaimInput> {
    let r: RewardClaimInput = serde_json::from_value(input.clone())
        .map_err(|_| GoFinError::validation("Invalid Morpho reward claim; specify wallet, network, chain_id, request_id, selected token addresses and max_fee_eth."))?;
    require(
        !r.request_id.trim().is_empty(),
        "REQUEST_ID_REQUIRED",
        "Reward claim requires a durable request identity.",
    )?;
    selected_tokens(&r.tokens)?;
    require(
        exact(&r.max_fee_eth, 18)? > U256::ZERO,
        "FEE_LIMIT",
        "Reward claim requires a positive finite native fee bound.",
    )?;
    Ok(r)
}

fn selected_tokens(tokens: &[String]) -> Result<BTreeSet<String>> {
    require(
        !tokens.is_empty() && tokens.len() <= 32,
        "MERKL_TOKEN_SELECTION_INVALID",
        "Select between one and 32 exact reward token addresses.",
    )?;
    let mut result = BTreeSet::new();
    for token in tokens {
        let token = address(token)?;
        require(
            token != Address::ZERO && result.insert(lower_address(token)),
            "MERKL_TOKEN_SELECTION_INVALID",
            "Reward tokens must be distinct nonzero addresses.",
        )?;
    }
    Ok(result)
}

fn parse_api_rewards(value: Value, chain_id: u64) -> Result<BTreeMap<String, ApiReward>> {
    let chains: Vec<ApiRewards> = serde_json::from_value(value).map_err(|_| {
        error(
            "MERKL_RESPONSE_INVALID",
            "read",
            "Merkl response is incomplete or malformed; rewards are unknown.",
        )
    })?;
    let mut result = BTreeMap::new();
    require(
        chains.len() <= 1,
        "MERKL_RESPONSE_INVALID",
        "Merkl returned multiple chain scopes for one explicit chain.",
    )?;
    for chain in chains {
        require(
            chain.chain.id == chain_id,
            "MERKL_CHAIN_MISMATCH",
            "Merkl returned rewards for a different chain.",
        )?;
        require(
            chain.rewards.len() <= 1024,
            "MERKL_RESPONSE_INVALID",
            "Merkl reward response exceeds the bounded read capacity.",
        )?;
        for reward in chain.rewards {
            let token = address(&reward.token.address)?;
            require(
                token != Address::ZERO,
                "MERKL_RESPONSE_INVALID",
                "Merkl returned a zero reward token.",
            )?;
            let cumulative = reward_units(&reward.amount)?;
            let claimed = reward_units(&reward.claimed)?;
            reward_units(&reward.pending)?;
            require(
                claimed <= cumulative && cumulative < (U256::from(1) << 208),
                "MERKL_RESPONSE_INVALID",
                "Merkl cumulative amounts exceed the reviewed distributor's uint208 range or claimed total.",
            )?;
            require(
                reward.proofs.len() <= 256,
                "MERKL_PROOF_INVALID",
                "Merkl proof exceeds the bounded tree depth.",
            )?;
            for proof in &reward.proofs {
                gofin_evm::hash(proof)?;
            }
            require(
                result.insert(lower_address(token), reward).is_none(),
                "MERKL_DUPLICATE_TOKEN",
                "Merkl returned duplicate wallet/token aggregates; do not sum cumulative claims.",
            )?;
        }
    }
    Ok(result)
}

fn reward_units(value: &str) -> Result<U256> {
    let quantity = Quantity::new(value)?;
    u256(&quantity)
}

fn reward_coverage(
    selected: Option<&BTreeSet<String>>,
    missing: &[String],
    verified: bool,
) -> Value {
    json!({"scope":if selected.is_some(){"explicit token selection"}else{"Merkl API enumeration"},"complete":selected.is_some() && missing.is_empty() && verified,"wallet_enumeration_complete":false,"missing_selected_tokens":missing,"source":"Merkl API reported entries; on-chain reads verify returned or explicitly selected identities, not all possible wallet entitlements","indexed_freshness":"unknown; proof usability checked against active on-chain root"})
}

fn verify_proof(
    owner: Address,
    token: Address,
    cumulative: U256,
    proofs: &[String],
    root: B256,
) -> Result<()> {
    require(
        root != B256::ZERO && proofs.len() <= 256,
        "MERKL_PROOF_INVALID",
        "Merkl active root is uninitialized or proof exceeds the reviewed bound.",
    )?;
    let mut current = keccak256((owner, token, cumulative).abi_encode());
    for sibling in proofs {
        let sibling = gofin_evm::hash(sibling)?;
        current = if current < sibling {
            keccak256((current, sibling).abi_encode())
        } else {
            keccak256((sibling, current).abi_encode())
        };
    }
    require(
        current == root,
        "MERKL_PROOF_INVALID",
        "Reward proof does not match the active on-chain Merkl root; refresh the claim data.",
    )
}

fn effective_recipient(
    owner: Address,
    token_recipient: Address,
    default_recipient: Address,
) -> Address {
    if token_recipient != Address::ZERO {
        token_recipient
    } else if default_recipient != Address::ZERO {
        default_recipient
    } else {
        owner
    }
}

fn encode_claim(owner: Address, claims: &[ClaimToken]) -> Result<Vec<u8>> {
    Ok(MerklDistributor::claimCall {
        users: vec![owner; claims.len()],
        tokens: claims
            .iter()
            .map(|c| address(&c.token.reference))
            .collect::<Result<_>>()?,
        amounts: claims
            .iter()
            .map(|c| u256(&c.cumulative))
            .collect::<Result<_>>()?,
        proofs: claims
            .iter()
            .map(|c| {
                c.proofs
                    .iter()
                    .map(|p| gofin_evm::hash(p))
                    .collect::<Result<Vec<_>>>()
            })
            .collect::<Result<_>>()?,
    }
    .abi_encode())
}

// On-chain review: immutable deployment observations on 2026-10-18 and verified
// Ethereum Distributor source at eth.blockscout.com. Base runtime is identical
// except the five immutable SELF addresses (offsets 2485,2558,3097,3161,3284).
// Upgrades fail closed pending review. Both implement abi.encode leaves, sorted
// proof pairs, uint208 cumulative claims, and token/default recipient precedence.
fn deployment(chain_id: u64) -> Result<(&'static str, &'static str, &'static str)> {
    match chain_id {
        1 => Ok((
            "0x33cc998fd4af3b6be42bac9a67fe97e9e275d2ae",
            "0x45a8f258094615c6b45b6fec0fab1bf25c6f28244b6c33a303d9ba952ec10d2d",
            "0x86c9090609b4bbddb98864b0eeb866e73d740904d312c7655ab6101246f8f3ea",
        )),
        8453 => Ok((
            "0x64455a45d85d872bfd7f833e367686108d13d6e6",
            "0xd26657d1944cb8c21e542285573d39cf37f7dcb70cb8e9d9fc71aec3875e19fc",
            "0xe2b8b259c681e55f0c264fcd7150ea5e21b05a20da26a12f0f02406be9bbd360",
        )),
        _ => Err(error(
            "MORPHO_NETWORK_UNSUPPORTED",
            "validation",
            "Morpho rewards support Ethereum and Base only.",
        )),
    }
}

impl EthereumAdapter {
    fn reward_network(&self, chain_id: u64, network: &str) -> Result<()> {
        deployment(chain_id)?;
        require(
            chain_id == self.config.chain_id && matches!(network, "ethereum" | "evm"),
            "WRONG_CHAIN",
            "Morpho reward network must match the configured immutable Ethereum chain.",
        )
    }

    async fn merkl_block(&self) -> Result<u64> {
        self.runtime.assert_chain().await?;
        let header = self
            .runtime
            .rpc
            .request("eth_getBlockByNumber", json!(["latest", false]))
            .await?;
        let timestamp = gofin_evm::rpc_u64(&header["timestamp"])?;
        let current = self.clock.now().timestamp();
        require(
            current >= 0
                && timestamp <= current as u64 + 30
                && timestamp.saturating_add(60) >= current as u64,
            "MERKL_STATE_STALE",
            "Merkl root and claimed-state observation requires a fresh canonical block.",
        )?;
        gofin_evm::hash(header["hash"].as_str().unwrap_or(""))?;
        gofin_evm::rpc_u64(&header["number"])
    }

    async fn verify_merkl_deployment(&self, block: u64) -> Result<()> {
        let (implementation, proxy_hash, implementation_hash) = deployment(self.config.chain_id)?;
        let slot = rpc_bytes(
            &self
                .runtime
                .rpc
                .request(
                    "eth_getStorageAt",
                    json!([DISTRIBUTOR, IMPLEMENTATION_SLOT, block_tag(Some(block))]),
                )
                .await?,
        )?;
        require(
            slot.len() == 32
                && slot[..12].iter().all(|v| *v == 0)
                && &slot[12..] == address(implementation)?.as_slice(),
            "MERKL_DEPLOYMENT_CHANGED",
            "Merkl distributor implementation differs from the reviewed deployment.",
        )?;
        for (contract, expected) in [
            (DISTRIBUTOR, proxy_hash),
            (implementation, implementation_hash),
        ] {
            let code = rpc_bytes(
                &self
                    .runtime
                    .rpc
                    .request("eth_getCode", json!([contract, block_tag(Some(block))]))
                    .await?,
            )?;
            require(
                !code.is_empty() && keccak256(code) == gofin_evm::hash(expected)?,
                "MERKL_DEPLOYMENT_CHANGED",
                "Merkl proxy or implementation runtime code differs from the reviewed identity.",
            )?;
        }
        Ok(())
    }

    async fn merkl_claimed_and_recipient(
        &self,
        owner: Address,
        token: Address,
        block: u64,
    ) -> Result<(U256, Address)> {
        let distributor = address(DISTRIBUTOR)?;
        let claimed = self
            .runtime
            .call(
                distributor,
                &MerklDistributor::claimedCall { user: owner, token },
                Some(block),
            )
            .await?;
        let token_recipient = self
            .runtime
            .call(
                distributor,
                &MerklDistributor::claimRecipientCall { user: owner, token },
                Some(block),
            )
            .await?;
        let default_recipient = if token_recipient == Address::ZERO {
            self.runtime
                .call(
                    distributor,
                    &MerklDistributor::claimRecipientCall {
                        user: owner,
                        token: Address::ZERO,
                    },
                    Some(block),
                )
                .await?
        } else {
            Address::ZERO
        };
        Ok((
            U256::from(claimed.amount),
            effective_recipient(owner, token_recipient, default_recipient),
        ))
    }

    pub(crate) async fn morpho_rewards_read(&self, input: &Value) -> Result<Value> {
        let r: RewardsReadInput = serde_json::from_value(input.clone())
            .map_err(|_| GoFinError::validation("Invalid Morpho rewards read input."))?;
        self.reward_network(r.chain_id, &r.network)?;
        require(
            r.wallet.is_some() || r.vault.is_some(),
            "MORPHO_REWARDS_SCOPE_REQUIRED",
            "Specify a vault for reward rates or a wallet for reward balances.",
        )?;
        let selected = r.tokens.as_deref().map(selected_tokens).transpose()?;
        require(
            selected.is_none() || r.wallet.is_some(),
            "MORPHO_REWARDS_SCOPE_REQUIRED",
            "Selected reward tokens require a wallet.",
        )?;
        let observed = now();
        let mut result = json!({"chain_id":r.chain_id,"network":r.network,"observed_at":observed,"coverage":"requested scope","aggregate_notice":AGGREGATE_NOTICE});
        if let Some(vault) = &r.vault {
            let vault = lower_address(address(vault)?);
            let response = self.morpho_api.graphql("query GoFinVaultV2Rewards($address: String!, $chainId: Int!) { vaultV2ByAddress(address: $address, chainId: $chainId) { address apy netApy rewards { supplyApr asset { address symbol } } } }", json!({"address":vault,"chainId":r.chain_id})).await?;
            require(
                response
                    .get("errors")
                    .is_none_or(|e| e.as_array().is_some_and(Vec::is_empty)),
                "MORPHO_REWARDS_DATA_UNAVAILABLE",
                "Morpho reward rate query returned errors; rates are unknown.",
            )?;
            let rates: VaultRewards = serde_json::from_value(response["vaultV2ByAddress"].clone())
                .map_err(|_| {
                    error(
                        "MORPHO_REWARDS_DATA_UNAVAILABLE",
                        "read",
                        "Morpho V2 aggregate reward rates are unavailable or incomplete.",
                    )
                })?;
            require(
                address(&rates.address)? == address(&vault)?,
                "MORPHO_VAULT_MISMATCH",
                "Reward rates belong to a different vault.",
            )?;
            for rate in &rates.rewards {
                address(&rate.asset.address)?;
            }
            result["vault"] = json!({"address":vault,"version":2,"source":"Morpho GraphQL indexed","rates":rates,"pre_aggregated":true,"net_apy_includes_rewards":true,"observed_at":observed});
        }
        if let Some(wallet_name) = &r.wallet {
            let owner = address(&self.wallet(wallet_name)?.address)?;
            let rows = parse_api_rewards(
                self.morpho_api
                    .merkl_rewards(r.chain_id, &lower_address(owner))
                    .await?,
                r.chain_id,
            )?;
            let missing = selected
                .as_ref()
                .map(|tokens| {
                    tokens
                        .iter()
                        .filter(|token| !rows.contains_key(*token))
                        .cloned()
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let block = self.merkl_block().await?;
            self.verify_merkl_deployment(block).await?;
            let root = self
                .runtime
                .call(
                    address(DISTRIBUTOR)?,
                    &MerklDistributor::getMerkleRootCall {},
                    Some(block),
                )
                .await?;
            let mut rewards = Vec::new();
            let mut claims_verified = true;
            for (reference, row) in rows {
                if selected
                    .as_ref()
                    .is_some_and(|tokens| !tokens.contains(&reference))
                {
                    continue;
                }
                let token = self.token(&reference, Some(block)).await?;
                require(
                    token.decimals == row.token.decimals,
                    "MERKL_TOKEN_MISMATCH",
                    "Merkl token decimals differ from on-chain metadata.",
                )?;
                let (claimed, recipient) = self
                    .merkl_claimed_and_recipient(owner, address(&reference)?, block)
                    .await?;
                let cumulative = reward_units(&row.amount)?;
                let proof_valid =
                    verify_proof(owner, address(&reference)?, cumulative, &row.proofs, root)
                        .is_ok();
                let claimable = if proof_valid && cumulative >= claimed && recipient == owner {
                    Some(quantity(cumulative - claimed))
                } else {
                    None
                };
                claims_verified &= claimable.is_some();
                rewards.push(json!({"token":token.asset(self),"cumulative_earned":quantity(cumulative),"claimed":quantity(claimed),"api_claimed":row.claimed,"pending":quantity(reward_units(&row.pending)?),"claimable":claimable,"proof_matches_active_root":proof_valid,"effective_recipient":lower_address(recipient),"recipient_matches_wallet":recipient==owner,"attribution":row.breakdowns,"claim_scope":"wallet_token_aggregate","source":"Merkl indexed rewards with pinned on-chain proof, claimed and recipient checks"}));
            }
            for reference in &missing {
                let token = self.token(reference, Some(block)).await?;
                let (claimed, recipient) = self
                    .merkl_claimed_and_recipient(owner, address(reference)?, block)
                    .await?;
                rewards.push(json!({"token":token.asset(self),"cumulative_earned":null,"claimed":quantity(claimed),"api_claimed":null,"pending":null,"claimable":null,"proof_matches_active_root":null,"effective_recipient":lower_address(recipient),"recipient_matches_wallet":recipient==owner,"attribution":null,"claim_scope":"wallet_token_aggregate","source":"On-chain claimed state and recipient only; selected token missing from Merkl API response","unavailable_reason":"MERKL_SELECTED_TOKEN_MISSING"}));
            }
            result["wallet"] = json!(wallet_name);
            result["resource"] = json!(resource(r.chain_id, owner));
            result["distributor"] = json!(DISTRIBUTOR);
            result["active_root"] = json!(format!("{root:#x}"));
            result["block_number"] = json!(block);
            result["coverage"] = reward_coverage(selected.as_ref(), &missing, claims_verified);
            result["rewards"] = json!(rewards);
        }
        Ok(result)
    }

    async fn merkl_claim_valuation(
        &self,
        transaction: &FrozenTransaction,
        claims: &[ClaimToken],
    ) -> Result<Valuation> {
        let unit = quantity(pow10(18)?);
        let (price, native_source, native_observed) = self
            .quote_verified_asset_usd_observed(&self.native_asset(), unit.clone())
            .await?;
        let fee = quantity(transaction.maximum_cost()?).mul_div_ceil(&price, &unit)?;
        let mut prices = vec![AssetPrice {
            asset: self.native_asset(),
            usd_micros_per_unit: price,
            unit_atoms: unit,
            observed_at: native_observed,
            source: native_source,
        }];
        let mut movement = Quantity::zero();
        for claim in claims {
            let unit = quantity(pow10(claim.token.decimals)?);
            let (price, source, observed_at) = self
                .quote_verified_asset_usd_observed(&claim.token, unit.clone())
                .await?;
            movement = movement.checked_add(&claim.payout.mul_div_ceil(&price, &unit)?)?;
            prices.push(AssetPrice {
                asset: claim.token.clone(),
                usd_micros_per_unit: price,
                unit_atoms: unit,
                observed_at,
                source,
            });
        }
        let observed_at = prices
            .iter()
            .map(|p| p.observed_at.clone())
            .min_by_key(|value| {
                chrono::DateTime::parse_from_rfc3339(value).expect("validated price timestamp")
            })
            .expect("native price is always present");
        let source = prices
            .iter()
            .map(|p| p.source.as_str())
            .collect::<Vec<_>>()
            .join("; ");
        Ok(Valuation {
            movement_usd_micros: movement,
            maximum_fee_usd_micros: fee,
            additional_allocation_usd_micros: Quantity::zero(),
            observed_at,
            source,
            prices,
        })
    }

    pub(crate) async fn morpho_claim_prepare(
        &self,
        input: &Value,
        _context: &PreparationContext,
    ) -> Result<PreparedOperation> {
        let r = parse_claim(input)?;
        self.reward_network(r.chain_id, &r.network)?;
        let wallet = self.wallet(&r.wallet)?;
        require(
            wallet.signing_enabled,
            "PROTECTED_SIGNER_REQUIRED",
            "Reward claims require the enrolled wallet's protected signer.",
        )?;
        let owner = address(&wallet.address)?;
        let selected = selected_tokens(&r.tokens)?;
        let rows = parse_api_rewards(
            self.morpho_api
                .merkl_rewards(r.chain_id, &lower_address(owner))
                .await?,
            r.chain_id,
        )?;
        let block = self.merkl_block().await?;
        self.verify_merkl_deployment(block).await?;
        let root = self
            .runtime
            .call(
                address(DISTRIBUTOR)?,
                &MerklDistributor::getMerkleRootCall {},
                Some(block),
            )
            .await?;
        let mut claims = Vec::new();
        for reference in selected {
            let row = rows.get(&reference).ok_or_else(|| {
                error(
                    "MERKL_REWARD_UNAVAILABLE",
                    "preview",
                    "A selected token has no Merkl reward aggregate; no claim was prepared.",
                )
            })?;
            let token = self.token(&reference, Some(block)).await?;
            require(
                token.decimals == row.token.decimals,
                "MERKL_TOKEN_MISMATCH",
                "Merkl token decimals differ from on-chain metadata.",
            )?;
            let cumulative = reward_units(&row.amount)?;
            verify_proof(owner, address(&reference)?, cumulative, &row.proofs, root)?;
            let (claimed, recipient) = self
                .merkl_claimed_and_recipient(owner, address(&reference)?, block)
                .await?;
            require(
                recipient == owner,
                "MERKL_RECIPIENT_REDIRECTED",
                "The stored Merkl recipient redirects this reward away from the enrolled wallet; change that setting separately before claiming.",
            )?;
            require(
                cumulative > claimed,
                "MERKL_NOTHING_CLAIMABLE",
                "The selected token has no positive reward remaining in the active root; pending rewards cannot be claimed.",
            )?;
            claims.push(ClaimToken {
                token: token.asset(self),
                cumulative: quantity(cumulative),
                previously_claimed: quantity(claimed),
                payout: quantity(cumulative - claimed),
                proofs: row.proofs.clone(),
            });
        }
        let prepared = self
            .runtime
            .prepare(
                owner,
                address(DISTRIBUTOR)?,
                U256::ZERO,
                encode_claim(owner, &claims)?.into(),
                exact(&r.max_fee_eth, 18)?,
            )
            .await?;
        let transaction = prepared.transaction;
        let valuation = self.merkl_claim_valuation(&transaction, &claims).await?;
        let resource = resource(r.chain_id, owner);
        let intent = FinancialIntent {
            debits: vec![],
            maximum_costs: vec![CostBound {
                location: AccountingLocation::Cash {
                    resource: resource.clone(),
                },
                amount: amount(&self.native_asset(), transaction.maximum_cost()?),
                treatment: CostTreatment::AdditionalDebit,
            }],
            expected: claims
                .iter()
                .map(|c| EffectConstraint::Credit {
                    to: resource.clone(),
                    asset: c.token.clone(),
                    minimum_credit: Quantity::zero(),
                })
                .collect(),
            locks: vec![resource.clone()],
        };
        let summary = format!(
            "Claim Merkl wallet/token aggregate rewards; estimated payouts: {}. Native fee at most {} ETH. {} {}",
            claims
                .iter()
                .map(|c| format!("{} atoms of {}", c.payout, c.token.reference))
                .collect::<Vec<_>>()
                .join(", "),
            r.max_fee_eth,
            AGGREGATE_NOTICE,
            PAYOUT_NOTICE
        );
        let binding = ClaimBinding {
            version: 2,
            transaction,
            resource,
            distributor: DISTRIBUTOR.into(),
            root: format!("{root:#x}"),
            claims,
            payout_semantics: Some(
                ClaimPayoutSemantics::CumulativeRemainderAtExecutionNoGuaranteedMinimum,
            ),
            value_bounds: None,
            boundary_authorization: None,
            financial_plan: None,
        };
        Ok(PreparedOperation { action_deadline: None, operation: CLAIM_OPERATION.into(), input: input.clone(), binding: serde_json::to_value(binding).expect("claim binding serializes"), summary, checks: vec![Check { name: "reviewed deployment, active proof root, cumulative claim and effective recipient".into(), status: CheckStatus::Passed, detail: format!("Pinned on-chain checks at block {block}; the locally encoded claim has no approvals, operators or callbacks.") }], warnings: vec![AGGREGATE_NOTICE.into(), PAYOUT_NOTICE.into()], observed_at: now(), expires_at: (self.clock.now() + chrono::Duration::seconds(120)).to_rfc3339(), valuation: Some(valuation), intent, operation_version: 1 })
    }

    fn verify_claim_binding(&self, prepared: &PreparedOperation) -> Result<ClaimBinding> {
        require(
            prepared.operation == CLAIM_OPERATION && prepared.operation_version == 1,
            "OPERATION_MISMATCH",
            "Merkl claim operation identity changed.",
        )?;
        let r = parse_claim(&prepared.input)?;
        self.reward_network(r.chain_id, &r.network)?;
        let b: ClaimBinding = serde_json::from_value(prepared.binding.clone())
            .map_err(|_| GoFinError::validation("Invalid Merkl claim binding."))?;
        require(
            serde_json::to_value(&b).ok().as_ref() == Some(&prepared.binding),
            "BINDING_INVALID",
            "Merkl claim binding is noncanonical.",
        )?;
        let wallet = self.wallet(&r.wallet)?;
        let owner = address(&wallet.address)?;
        let selected = selected_tokens(&r.tokens)?;
        let references = b
            .claims
            .iter()
            .map(|c| c.token.reference.clone())
            .collect::<BTreeSet<_>>();
        require(
            matches!(
                (b.version, &b.payout_semantics),
                (1, None)
                    | (
                        2,
                        Some(
                            ClaimPayoutSemantics::CumulativeRemainderAtExecutionNoGuaranteedMinimum
                        )
                    )
            ) && b.distributor == DISTRIBUTOR
                && b.resource == resource(r.chain_id, owner)
                && b.transaction.chain_id == r.chain_id
                && b.transaction.sender == lower_address(owner)
                && b.transaction.to == DISTRIBUTOR
                && b.transaction.value.is_zero()
                && selected == references
                && references.len() == b.claims.len()
                && b.transaction.maximum_cost()? <= exact(&r.max_fee_eth, 18)?,
            "BINDING_CHANGED",
            "Merkl signer, chain, distributor, selected tokens or fee differs from the approved scope.",
        )?;
        for claim in &b.claims {
            require(
                claim.token == self.asset(&claim.token.reference, claim.token.decimals)
                    && claim.cumulative > claim.previously_claimed
                    && claim.cumulative.checked_sub(&claim.previously_claimed)? == claim.payout
                    && u256(&claim.cumulative)? < (U256::from(1) << 208),
                "BINDING_CHANGED",
                "Merkl reward identity or cumulative arithmetic changed.",
            )?;
            verify_proof(
                owner,
                address(&claim.token.reference)?,
                u256(&claim.cumulative)?,
                &claim.proofs,
                gofin_evm::hash(&b.root)?,
            )?;
        }
        require(
            rpc_bytes(&json!(b.transaction.calldata))? == encode_claim(owner, &b.claims)?,
            "BINDING_CHANGED",
            "Merkl transaction calldata differs from the exact locally encoded approved claim.",
        )?;
        Ok(b)
    }

    pub(crate) async fn morpho_claim_revalidate(
        &self,
        prepared: &PreparedOperation,
    ) -> Result<PreparedOperation> {
        let b = self.verify_claim_binding(prepared)?;
        let input = parse_claim(&prepared.input)?;
        require(
            self.wallet(&input.wallet)?.signing_enabled,
            "PROTECTED_SIGNER_REQUIRED",
            "Reward claims require the enrolled wallet's protected signer.",
        )?;
        let owner = address(&b.transaction.sender)?;
        let block = self.merkl_block().await?;
        self.verify_merkl_deployment(block).await?;
        let root = self
            .runtime
            .call(
                address(DISTRIBUTOR)?,
                &MerklDistributor::getMerkleRootCall {},
                Some(block),
            )
            .await?;
        require(
            root == gofin_evm::hash(&b.root)?,
            "MERKL_ROOT_CHANGED",
            "Merkl's active root changed; prepare a fresh candidate within existing authority.",
        )?;
        for claim in &b.claims {
            let token = self.token(&claim.token.reference, Some(block)).await?;
            let (claimed, recipient) = self
                .merkl_claimed_and_recipient(owner, address(&claim.token.reference)?, block)
                .await?;
            require(
                token.asset(self) == claim.token && quantity(claimed) == claim.previously_claimed,
                "MERKL_CLAIM_CHANGED",
                "Reward token identity or claimed amount changed since preview.",
            )?;
            require(
                recipient == owner,
                "MERKL_RECIPIENT_REDIRECTED",
                "The effective Merkl recipient changed away from the enrolled wallet.",
            )?;
        }
        self.runtime.revalidate(&b.transaction).await?;
        let mut fresh = prepared.clone();
        fresh.observed_at = now();
        fresh.expires_at = (self.clock.now() + chrono::Duration::seconds(120)).to_rfc3339();
        fresh.valuation = Some(
            self.merkl_claim_valuation(&b.transaction, &b.claims)
                .await?,
        );
        carry_attempt_valuation(prepared, &mut fresh, self.clock.now())?;
        gofin_domain::value_bounds::validate(&fresh)?;
        Ok(fresh)
    }

    pub(crate) async fn morpho_claim_submit_revalidated(
        &self,
        validated: &RevalidatedOperation,
        journal: Arc<dyn SubmissionJournal>,
    ) -> Result<Submission> {
        validated.assert_fresh()?;
        let b = self.verify_claim_binding(validated.prepared())?;
        self.runtime.submit_revalidated(&b.transaction, json!({"operation":CLAIM_OPERATION,"resource":b.resource,"distributor":b.distributor,"root":b.root}), journal, validated).await
    }

    pub(crate) async fn morpho_claim_reconcile(
        &self,
        record: &OperationRecord,
    ) -> Result<Reconciliation> {
        let submission = record.submission.as_ref().ok_or_else(|| {
            error(
                "SUBMISSION_REQUIRED",
                "reconciliation",
                "Original Merkl claim submission identity is missing.",
            )
        })?;
        let preview = record.preview.as_ref().ok_or_else(|| {
            error(
                "BINDING_REQUIRED",
                "reconciliation",
                "Original Merkl claim preview is missing.",
            )
        })?;
        require(
            record.operation == CLAIM_OPERATION
                && preview.operation == record.operation
                && preview.input == record.input,
            "BINDING_IDENTITY_MISMATCH",
            "Merkl claim record differs from its original preview.",
        )?;
        let b = self.verify_claim_binding(preview)?;
        let submitted: FrozenTransaction =
            serde_json::from_value(submission.data["transaction"].clone()).map_err(|_| {
                error(
                    "SUBMISSION_IDENTITY_MISMATCH",
                    "reconciliation",
                    "Merkl submitted envelope is malformed.",
                )
            })?;
        require(
            submitted == b.transaction
                && submission.data["metadata"]
                    == json!({"operation":CLAIM_OPERATION,"resource":b.resource,"distributor":b.distributor,"root":b.root}),
            "SUBMISSION_IDENTITY_MISMATCH",
            "Merkl submission differs from its original frozen envelope.",
        )?;
        let Some(receipt) = self
            .runtime
            .receipt(submission, &b.transaction, self.config.confirmations)
            .await?
        else {
            return Ok(pending_claim(submission, "awaiting-canonical-receipt"));
        };
        if self.config.requires_rpc_finalized() && !self.merkl_finalized(&receipt).await? {
            return Ok(pending_claim(submission, "awaiting-rpc-finalized"));
        }
        let mut effects = vec![FinancialEffect::Cost {
            location: AccountingLocation::Cash {
                resource: b.resource.clone(),
            },
            amount: amount(&self.native_asset(), receipt.fee),
            category: "ethereum_gas".into(),
            treatment: CostTreatment::AdditionalDebit,
        }];
        let mut actual_payouts = BTreeMap::new();
        if receipt.success {
            self.verify_merkl_deployment(receipt.block_number).await?;
            actual_payouts = verify_claim_events(&receipt.receipt, &b)?;
            for claim in &b.claims {
                let state = self
                    .runtime
                    .call(
                        address(DISTRIBUTOR)?,
                        &MerklDistributor::claimedCall {
                            user: address(&b.transaction.sender)?,
                            token: address(&claim.token.reference)?,
                        },
                        Some(receipt.block_number),
                    )
                    .await?;
                require(
                    U256::from(state.amount) >= u256(&claim.cumulative)?,
                    "MERKL_PROOF_MISMATCH",
                    "Canonical Merkl claimed state does not contain the settled cumulative entitlement.",
                )?;
                let payout = actual_payouts[&claim.token.reference].clone();
                if !payout.is_zero() {
                    effects.push(FinancialEffect::UsableCredit {
                        to: b.resource.clone(),
                        amount: Amount {
                            asset: claim.token.clone(),
                            atoms: payout,
                        },
                    });
                }
            }
        } else {
            require(
                merkl_events::<MerklDistributor::Claimed>(&receipt.receipt, address(DISTRIBUTOR)?)?
                    .is_empty(),
                "MERKL_PROOF_MISMATCH",
                "Reverted Merkl claim unexpectedly includes claim events.",
            )?;
        }
        if !self.inclusion_still_canonical(&receipt).await? {
            return Ok(pending_claim(submission, "inclusion-changed"));
        }
        let evidence = json!({"original_hash":submission.reference,"block_number":receipt.block_number,"block_hash":format!("{:#x}",receipt.block_hash),"success":receipt.success,"fee_wei":quantity(receipt.fee),"distributor":DISTRIBUTOR,"root":b.root,"claims":b.claims,"actual_payouts":actual_payouts,"payout_semantics":b.payout_semantics,"claim_scope":"wallet_token_aggregate","canonical_transaction_effects_verified":true,"finality":self.config.finality_label()});
        let mut verified = VerifiedEffects {
            evidence_version: 1,
            submission: submission.clone(),
            effects,
            evidence: evidence.clone(),
            observed_at: now(),
            proof_digest: String::new(),
        };
        verified.proof_digest = verified_effect_digest(&verified)?;
        Ok(Reconciliation {
            status: if receipt.success {
                Status::Completed
            } else {
                Status::Failed
            },
            evidence,
            verified: Some(verified),
        })
    }

    async fn merkl_finalized(&self, receipt: &CanonicalReceipt) -> Result<bool> {
        let finalized = self
            .runtime
            .rpc
            .request("eth_getBlockByNumber", json!(["finalized", false]))
            .await;
        Ok(finalized
            .as_ref()
            .ok()
            .filter(|v| !v.is_null())
            .and_then(|v| gofin_evm::rpc_u64(&v["number"]).ok())
            .is_some_and(|height| height >= receipt.block_number))
    }

    pub(crate) async fn morpho_claim_plan_permission(
        &self,
        input: &Value,
        raw: &Value,
    ) -> Result<PlanPermission> {
        let r = parse_claim(input)?;
        self.reward_network(r.chain_id, &r.network)?;
        let wallet = self.wallet(&r.wallet)?;
        require(
            wallet.signing_enabled,
            "PROTECTED_SIGNER_REQUIRED",
            "Reward claim plan requires an enrolled signer.",
        )?;
        let owner = address(&wallet.address)?;
        let selected = selected_tokens(&r.tokens)?;
        let bounds: RewardClaimBounds = serde_json::from_value(raw.clone()).map_err(|_| {
            GoFinError::validation(
                "Reward claim bounds require max_amounts by exact token address and max_fee_eth.",
            )
        })?;
        let fee = exact(&bounds.max_fee_eth, 18)?;
        require(
            fee > U256::ZERO && exact(&r.max_fee_eth, 18)? <= fee,
            "MERKL_BOUNDS_INVALID",
            "Reward claim fee exceeds the finite plan bound.",
        )?;
        let normalized = bounds
            .max_amounts
            .iter()
            .map(|(token, amount)| Ok((lower_address(address(token)?), amount.clone())))
            .collect::<Result<BTreeMap<_, _>>>()?;
        require(
            normalized.len() == bounds.max_amounts.len()
                && normalized.keys().cloned().collect::<BTreeSet<_>>() == selected,
            "MERKL_BOUNDS_INVALID",
            "Reward claim bounds must cover exactly the selected tokens with no duplicate identities.",
        )?;
        let block = self.merkl_block().await?;
        self.verify_merkl_deployment(block).await?;
        let resource = resource(r.chain_id, owner);
        let mut maximums = BTreeMap::new();
        let mut identities = Vec::new();
        let mut token_assets = Vec::new();
        for reference in selected {
            let token = self.token(&reference, Some(block)).await?;
            let maximum = Quantity::from_decimal(&normalized[&reference], token.decimals)?;
            require(
                !maximum.is_zero() && u256(&maximum)? < (U256::from(1) << 208),
                "MERKL_BOUNDS_INVALID",
                "Each reward claim requires a positive finite payout ceiling.",
            )?;
            maximums.insert(reference, maximum);
            identities.push(FinancialPlanFundingIdentity {
                resource: resource.clone(),
                asset: token.asset(self),
            });
            token_assets.push(token.asset(self));
        }
        let deferred_receipt = if token_assets.len() == 1 {
            Some(ResourceAmount {
                resource: resource.clone(),
                amount: Amount {
                    asset: token_assets[0].clone(),
                    atoms: maximums[&token_assets[0].reference].clone(),
                },
            })
        } else {
            None
        };
        Ok(PlanPermission {
            recovery_parent: None,
            spec: BoundarySpec {
                fixed: json!({"operation":CLAIM_OPERATION,"input":claim_fixed_input(input),"signer":lower_address(owner),"distributor":DISTRIBUTOR,"tokens":token_assets,"payout_semantics":ClaimPayoutSemantics::CumulativeRemainderAtExecutionNoGuaranteedMinimum}),
                parameters: json!({"maximums":maximums,"max_fee_eth":bounds.max_fee_eth}),
                maximum_debits: vec![],
                maximum_costs: vec![CostBound {
                    location: AccountingLocation::Cash { resource },
                    amount: amount(&self.native_asset(), fee),
                    treatment: CostTreatment::AdditionalDebit,
                }],
                summary: format!(
                    "Claim selected Merkl wallet/token aggregates within token payout ceilings {} and fee {} ETH. {} {}",
                    serde_json::to_string(&bounds.max_amounts).expect("map serializes"),
                    bounds.max_fee_eth,
                    AGGREGATE_NOTICE,
                    PAYOUT_NOTICE
                ),
            },
            possible_credits: identities,
            fee_assets: vec![self.native_asset()],
            deferred_receipt,
            authorization_seconds: None,
            cumulative_scope: None,
        })
    }

    pub(crate) fn morpho_claim_validate_candidate(
        &self,
        spec: &BoundarySpec,
        prepared: &PreparedOperation,
    ) -> Result<()> {
        let b = self.verify_claim_binding(prepared)?;
        let r = parse_claim(&prepared.input)?;
        require(
            spec.fixed["operation"] == json!(CLAIM_OPERATION)
                && spec.fixed["input"] == claim_fixed_input(&prepared.input)
                && spec.fixed["signer"] == json!(b.transaction.sender)
                && spec.fixed["distributor"] == json!(DISTRIBUTOR)
                && spec.fixed["payout_semantics"] == json!(b.payout_semantics)
                && spec.fixed["tokens"]
                    == json!(b.claims.iter().map(|c| &c.token).collect::<Vec<_>>()),
            "BOUNDARY_VIOLATION",
            "Reward claim changed fixed wallet, chain, distributor or selected token scope.",
        )?;
        let maximums: BTreeMap<String, Quantity> =
            serde_json::from_value(spec.parameters["maximums"].clone())
                .map_err(|_| GoFinError::validation("Invalid Merkl payout bounds."))?;
        for claim in &b.claims {
            require(
                maximums
                    .get(&claim.token.reference)
                    .is_some_and(|cap| &claim.payout <= cap),
                "BOUNDARY_VIOLATION",
                "Reward payout exceeds its approved token ceiling.",
            )?;
        }
        require(
            exact(&r.max_fee_eth, 18)?
                <= exact(spec.parameters["max_fee_eth"].as_str().unwrap_or(""), 18)?,
            "BOUNDARY_VIOLATION",
            "Reward claim fee exceeds plan authority.",
        )
    }
}

fn claim_fixed_input(input: &Value) -> Value {
    let mut fixed = input.clone();
    if let Some(object) = fixed.as_object_mut() {
        object.remove("request_id");
        object.remove("max_fee_eth");
    }
    fixed
}
fn pending_claim(submission: &Submission, stage: &str) -> Reconciliation {
    Reconciliation {
        status: Status::Pending,
        evidence: json!({"original_hash":submission.reference,"stage":stage}),
        verified: None,
    }
}

fn merkl_events<E: SolEvent>(receipt: &Value, emitter: Address) -> Result<Vec<E>> {
    let logs = receipt["logs"].as_array().ok_or_else(|| {
        error(
            "RECEIPT_LOGS_INVALID",
            "reconciliation",
            "Canonical reward receipt has no log array.",
        )
    })?;
    let mut found = Vec::new();
    for log in logs {
        if address(log["address"].as_str().unwrap_or(""))? != emitter {
            continue;
        }
        let topics = log["topics"]
            .as_array()
            .ok_or_else(|| {
                error(
                    "RECEIPT_LOGS_INVALID",
                    "reconciliation",
                    "Reward receipt log topics are missing.",
                )
            })?
            .iter()
            .map(|t| gofin_evm::hash(t.as_str().unwrap_or("")))
            .collect::<Result<Vec<_>>>()?;
        if topics.first() != Some(&E::SIGNATURE_HASH) {
            continue;
        }
        require(
            log["removed"].as_bool() != Some(true)
                && log["transactionHash"] == receipt["transactionHash"]
                && log["blockHash"] == receipt["blockHash"]
                && log["blockNumber"] == receipt["blockNumber"],
            "RECEIPT_LOG_IDENTITY",
            "Reward event inclusion differs from the canonical transaction.",
        )?;
        found.push(
            E::decode_raw_log_validate(topics.iter().copied(), &rpc_bytes(&log["data"])?).map_err(
                |_| {
                    error(
                        "RECEIPT_EVENT_INVALID",
                        "reconciliation",
                        "Reward event does not match the reviewed ABI.",
                    )
                },
            )?,
        );
    }
    Ok(found)
}

fn verify_claim_events(
    receipt: &Value,
    binding: &ClaimBinding,
) -> Result<BTreeMap<String, Quantity>> {
    let owner = address(&binding.transaction.sender)?;
    let distributor = address(DISTRIBUTOR)?;
    let events = merkl_events::<MerklDistributor::Claimed>(receipt, distributor)?;
    require(
        events.len() == binding.claims.len(),
        "MERKL_PROOF_MISMATCH",
        "Canonical reward claim includes missing, duplicate or unexpected tokens.",
    )?;
    let mut payouts = BTreeMap::new();
    for claim in &binding.claims {
        let token = address(&claim.token.reference)?;
        let expected = u256(&claim.payout)?;
        let matching = events
            .iter()
            .filter(|e| e.user == owner && e.token == token)
            .collect::<Vec<_>>();
        require(
            matching.len() == 1
                && if binding.version == 1 {
                    matching[0].amount == expected
                } else {
                    matching[0].amount <= expected
                },
            "MERKL_PROOF_MISMATCH",
            "Canonical Claimed event exceeds the approved incremental payout or conflicts with its exact legacy binding.",
        )?;
        let actual = matching[0].amount;
        let transfers = merkl_events::<crate::abi::Erc20::Transfer>(receipt, token)?;
        let relevant = transfers
            .iter()
            .filter(|e| e.from == owner || e.to == owner)
            .collect::<Vec<_>>();
        require(
            if actual.is_zero() {
                relevant.is_empty()
            } else {
                relevant.len() == 1
                    && relevant[0].from == distributor
                    && relevant[0].to == owner
                    && relevant[0].value == actual
            },
            "MERKL_PROOF_MISMATCH",
            "Claimed reward lacks its exact transaction-specific ERC-20 credit to the enrolled wallet.",
        )?;
        payouts.insert(claim.token.reference.clone(), quantity(actual));
    }
    Ok(payouts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gofin_evm::EvmRpc;
    use std::sync::{
        Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    };

    // Public deployed bytes at Ethereum block 26218595 / 0xcbf8360fa0729443b0cc560ef55b4d8700541de2c736f23a4fbcbee1a6bc3c10.
    // These exercise the real production deployment verifier; no test bypass.
    struct RewardRpc {
        root: Mutex<B256>,
        claimed: AtomicU64,
        recipient: Mutex<Address>,
        stale: AtomicBool,
        changed_code: AtomicBool,
        transaction: Mutex<Option<FrozenTransaction>>,
        receipt_payout: AtomicU64,
        settled: AtomicBool,
    }
    impl RewardRpc {
        fn new() -> Self {
            Self {
                root: Mutex::new(gofin_evm::hash(&binding().root).unwrap()),
                claimed: AtomicU64::new(20),
                recipient: Mutex::new(Address::ZERO),
                stale: AtomicBool::new(false),
                changed_code: AtomicBool::new(false),
                transaction: Mutex::new(None),
                receipt_payout: AtomicU64::new(103),
                settled: AtomicBool::new(false),
            }
        }
    }
    impl RewardRpc {
        fn canonical_receipt(&self) -> Value {
            let payout = U256::from(self.receipt_payout.load(Ordering::SeqCst));
            let mut logs = vec![event_log(
                &MerklDistributor::Claimed {
                    user: owner(),
                    token: token(),
                    amount: payout,
                },
                address(DISTRIBUTOR).unwrap(),
            )];
            if !payout.is_zero() {
                logs.push(event_log(
                    &crate::abi::Erc20::Transfer {
                        from: address(DISTRIBUTOR).unwrap(),
                        to: owner(),
                        value: payout,
                    },
                    token(),
                ));
            }
            for log in &mut logs {
                log["transactionHash"] = json!(format!("{:#x}", B256::repeat_byte(0xaa)));
                log["blockHash"] = json!(format!("{:#x}", B256::repeat_byte(0xbb)));
                log["blockNumber"] = json!("0x14");
            }
            json!({"transactionHash":format!("{:#x}", B256::repeat_byte(0xaa)),"blockHash":format!("{:#x}", B256::repeat_byte(0xbb)),"blockNumber":"0x14","status":"0x1","gasUsed":"0x186a0","effectiveGasPrice":"0x1","logs":logs})
        }
    }
    #[async_trait]
    impl EvmRpc for RewardRpc {
        async fn request(&self, method: &str, params: Value) -> Result<Value> {
            let encode = |value: Vec<u8>| json!(gofin_evm::hex_bytes(&value));
            Ok(match method {
                "eth_chainId" => json!("0x1"),
                "eth_getBlockByNumber" => {
                    json!({"number":"0x14","hash":format!("{:#x}",B256::repeat_byte(0xbb)),"timestamp":format!("{:#x}",chrono::Utc::now().timestamp() - if self.stale.load(Ordering::SeqCst) { 300 } else { 0 }),"baseFeePerGas":"0x1"})
                }
                "eth_blockNumber" => json!("0x15"),
                "eth_maxPriorityFeePerGas" => json!("0x1"),
                "eth_getTransactionCount" => json!("0x7"),
                "eth_estimateGas" => json!("0x186a0"),
                "eth_getBalance" => json!("0xde0b6b3a7640000"),
                "eth_getTransactionReceipt" if !self.settled.load(Ordering::SeqCst) => Value::Null,
                "eth_getTransactionReceipt" => self.canonical_receipt(),
                "eth_getTransactionByHash" => {
                    let frozen = self.transaction.lock().unwrap();
                    let t = frozen.as_ref().unwrap();
                    json!({"hash":format!("{:#x}",B256::repeat_byte(0xaa)),"from":t.sender,"to":t.to,"chainId":"0x1","type":"0x2","nonce":format!("{:#x}",t.nonce),"gas":format!("{:#x}",t.gas),"value":"0x0","input":t.calldata,"maxFeePerGas":format!("{:#x}",u256(&t.max_fee_per_gas)?),"maxPriorityFeePerGas":format!("{:#x}",u256(&t.max_priority_fee_per_gas)?),"accessList":[],"blockHash":format!("{:#x}",B256::repeat_byte(0xbb)),"blockNumber":"0x14"})
                }
                "eth_getStorageAt" => encode(address(deployment(1)?.0)?.into_word().abi_encode()),
                "eth_getCode" if self.changed_code.load(Ordering::SeqCst) => json!("0x6000"),
                "eth_getCode" if params[0] == DISTRIBUTOR => {
                    json!(include_str!("testdata/merkl-proxy.hex").trim())
                }
                "eth_getCode" if params[0] == deployment(1)?.0 => {
                    json!(include_str!("testdata/merkl-implementation.hex").trim())
                }
                "eth_getCode" => json!("0x6000"),
                "eth_call" => {
                    let data = rpc_bytes(&params[0]["data"])?;
                    if data.starts_with(&MerklDistributor::claimCall::SELECTOR) {
                        json!("0x")
                    } else if data.starts_with(&MerklDistributor::getMerkleRootCall::SELECTOR) {
                        encode(self.root.lock().unwrap().abi_encode())
                    } else if data.starts_with(&MerklDistributor::claimedCall::SELECTOR) {
                        encode(
                            (
                                U256::from(self.claimed.load(Ordering::SeqCst)),
                                U256::ZERO,
                                B256::ZERO,
                            )
                                .abi_encode(),
                        )
                    } else if data.starts_with(&MerklDistributor::claimRecipientCall::SELECTOR) {
                        encode(self.recipient.lock().unwrap().abi_encode())
                    } else if data.starts_with(&crate::abi::Erc20::decimalsCall::SELECTOR) {
                        encode(U256::from(6).abi_encode())
                    } else if data.starts_with(&crate::abi::Erc20::symbolCall::SELECTOR) {
                        encode("TEST".to_string().abi_encode())
                    } else {
                        return Err(GoFinError::validation(
                            "Unexpected reward fixture call; no dispatch allowed.",
                        ));
                    }
                }
                _ => {
                    return Err(GoFinError::validation(
                        "Unexpected reward fixture RPC; no dispatch allowed.",
                    ));
                }
            })
        }
    }
    struct RewardApi(Value);
    #[async_trait]
    impl super::super::api::MorphoApiTransport for RewardApi {
        async fn request(&self, request: super::super::api::MorphoApiRequest) -> Result<Value> {
            assert!(matches!(
                request,
                super::super::api::MorphoApiRequest::MerklRewards { chain_id: 1, .. }
            ));
            if self.0.is_null() {
                return Err(GoFinError::new(
                    "API_UNAVAILABLE",
                    "read",
                    "Synthetic outage.",
                    "Retry.",
                ));
            }
            Ok(self.0.clone())
        }
    }
    fn rpc_adapter(rpc: Arc<RewardRpc>, response: Value) -> EthereumAdapter {
        EthereumAdapter::new(
            EthereumConfig {
                chain_id: 1,
                ..Default::default()
            },
            vec![WalletConnection {
                name: "synthetic".into(),
                address: lower_address(owner()),
                signing_enabled: true,
            }],
            Arc::new(EvmRuntime::new(rpc, 1, None)),
        )
        .unwrap()
        .with_morpho_api(super::super::api::MorphoClient::with_transport(Arc::new(
            RewardApi(response),
        )))
    }
    fn api_response() -> Value {
        json!([{"chain":{"id":1},"rewards":[{"token":{"address":lower_address(token()),"decimals":6},"amount":"123","claimed":"20","pending":"50","proofs":[]}]}])
    }
    #[tokio::test]
    async fn merkl_selected_missing_token_retains_unknown_values_and_independent_claimed_state() {
        let adapter = rpc_adapter(Arc::new(RewardRpc::new()), json!([]));
        let result = adapter.morpho_rewards_read(&json!({"network":"evm","chain_id":1,"wallet":"synthetic","tokens":[lower_address(token())]})).await.unwrap();
        let row = &result["rewards"][0];
        assert_eq!(result["coverage"]["complete"], false);
        assert_eq!(row["claimed"], "20");
        assert_eq!(row["effective_recipient"], lower_address(owner()));
        for field in ["cumulative_earned", "pending", "claimable", "api_claimed"] {
            assert!(row[field].is_null(), "{field} must remain unknown");
        }
        assert_eq!(
            adapter
                .morpho_claim_prepare(&prepared_claim().input, &PreparationContext::default())
                .await
                .unwrap_err()
                .code,
            "MERKL_REWARD_UNAVAILABLE"
        );
        let outage = rpc_adapter(Arc::new(RewardRpc::new()), Value::Null);
        assert_eq!(
            outage
                .morpho_rewards_read(
                    &json!({"network":"ethereum","chain_id":1,"wallet":"synthetic"})
                )
                .await
                .unwrap_err()
                .code,
            "API_UNAVAILABLE"
        );
    }
    #[tokio::test]
    async fn merkl_revalidation_rejects_root_claimed_recipient_code_and_stale_state_changes() {
        for (scenario, code) in [
            ("root", "MERKL_ROOT_CHANGED"),
            ("claimed", "MERKL_CLAIM_CHANGED"),
            ("recipient", "MERKL_RECIPIENT_REDIRECTED"),
            ("code", "MERKL_DEPLOYMENT_CHANGED"),
            ("stale", "MERKL_STATE_STALE"),
        ] {
            let rpc = Arc::new(RewardRpc::new());
            match scenario {
                "root" => *rpc.root.lock().unwrap() = B256::repeat_byte(0x99),
                "claimed" => rpc.claimed.store(21, Ordering::SeqCst),
                "recipient" => *rpc.recipient.lock().unwrap() = token(),
                "code" => rpc.changed_code.store(true, Ordering::SeqCst),
                "stale" => rpc.stale.store(true, Ordering::SeqCst),
                _ => unreachable!(),
            }
            let adapter = rpc_adapter(rpc, api_response());
            assert_eq!(
                adapter
                    .morpho_claim_revalidate(&prepared_claim())
                    .await
                    .unwrap_err()
                    .code,
                code,
                "{scenario}"
            );
        }
    }
    #[tokio::test]
    async fn merkl_pending_rewards_cannot_make_an_exhausted_cumulative_entitlement_spendable() {
        let rpc = Arc::new(RewardRpc::new());
        rpc.claimed.store(123, Ordering::SeqCst);
        let adapter = rpc_adapter(rpc, api_response());
        let result = adapter.morpho_rewards_read(&json!({"network":"ethereum","chain_id":1,"wallet":"synthetic","tokens":[lower_address(token())]})).await.unwrap();
        assert_eq!(result["rewards"][0]["pending"], "50");
        assert_eq!(result["rewards"][0]["claimable"], "0");
        assert_eq!(
            adapter
                .morpho_claim_prepare(&prepared_claim().input, &PreparationContext::default())
                .await
                .unwrap_err()
                .code,
            "MERKL_NOTHING_CLAIMABLE"
        );
    }
    fn owner() -> Address {
        address("0x0000000000000000000000000000000000000011").unwrap()
    }
    fn token() -> Address {
        address("0x0000000000000000000000000000000000000022").unwrap()
    }

    #[test]
    fn merkl_proof_binds_abi_encoded_owner_token_cumulative_and_sorted_pairs() {
        let cumulative = U256::from(123);
        let leaf = keccak256((owner(), token(), cumulative).abi_encode());
        let sibling = keccak256(b"another leaf");
        let root = if leaf < sibling {
            keccak256((leaf, sibling).abi_encode())
        } else {
            keccak256((sibling, leaf).abi_encode())
        };
        let proof = vec![format!("{sibling:#x}")];
        assert!(verify_proof(owner(), token(), cumulative, &proof, root).is_ok());
        assert!(verify_proof(token(), owner(), cumulative, &proof, root).is_err());
        assert!(verify_proof(owner(), token(), U256::from(122), &proof, root).is_err());
        assert!(verify_proof(owner(), token(), cumulative, &proof, B256::ZERO).is_err());
        assert!(verify_proof(owner(), token(), cumulative, &[], leaf).is_ok());
    }

    #[test]
    fn merkl_recipient_token_override_precedes_default_and_wallet() {
        assert_eq!(
            effective_recipient(owner(), Address::ZERO, Address::ZERO),
            owner()
        );
        assert_eq!(
            effective_recipient(owner(), Address::ZERO, token()),
            token()
        );
        assert_eq!(effective_recipient(owner(), owner(), token()), owner());
        assert_eq!(effective_recipient(owner(), token(), owner()), token());
    }

    fn api_row() -> Value {
        json!({"token":{"address":lower_address(token()),"decimals":6},"amount":"123","claimed":"20","pending":"50","proofs":[]})
    }

    #[test]
    fn merkl_response_rejects_duplicate_aggregates_wrong_chain_and_missing_data() {
        let row = api_row();
        let good = json!([{"chain":{"id":8453},"rewards":[row.clone()]}]);
        let parsed = parse_api_rewards(good, 8453).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed.values().next().unwrap().pending, "50");
        assert!(
            parse_api_rewards(
                json!([{"chain":{"id":8453},"rewards":[row.clone(),row.clone()]}]),
                8453
            )
            .is_err()
        );
        assert!(
            parse_api_rewards(json!([{"chain":{"id":1},"rewards":[row.clone()]}]), 8453).is_err()
        );
        assert!(parse_api_rewards(json!({"error":"offline"}), 8453).is_err());
        let mut missing = row.clone();
        missing.as_object_mut().unwrap().remove("pending");
        assert!(
            parse_api_rewards(json!([{"chain":{"id":8453},"rewards":[missing]}]), 8453).is_err()
        );
        let mut overclaimed = row;
        overclaimed["claimed"] = json!("124");
        assert!(
            parse_api_rewards(json!([{"chain":{"id":8453},"rewards":[overclaimed]}]), 8453)
                .is_err()
        );
    }

    #[test]
    fn merkl_selection_is_explicit_finite_and_deduplicated() {
        assert!(selected_tokens(&[]).is_err());
        assert!(selected_tokens(&[lower_address(Address::ZERO)]).is_err());
        assert!(selected_tokens(&[lower_address(token()), lower_address(token())]).is_err());
        assert_eq!(selected_tokens(&[lower_address(token())]).unwrap().len(), 1);
        assert!(deployment(42161).is_err());
    }

    fn binding() -> ClaimBinding {
        let cumulative = U256::from(123);
        let root = keccak256((owner(), token(), cumulative).abi_encode());
        let claim = ClaimToken {
            token: AssetId {
                network: "ethereum:1".into(),
                reference: lower_address(token()),
                decimals: 6,
            },
            cumulative: quantity(cumulative),
            previously_claimed: quantity(U256::from(20)),
            payout: quantity(U256::from(103)),
            proofs: vec![],
        };
        let calldata = encode_claim(owner(), std::slice::from_ref(&claim)).unwrap();
        ClaimBinding {
            version: 2,
            transaction: FrozenTransaction {
                chain_id: 1,
                sender: lower_address(owner()),
                to: DISTRIBUTOR.into(),
                value: Quantity::zero(),
                calldata: format!("0x{}", hex::encode(calldata)),
                nonce: 7,
                gas: 200000,
                max_fee_per_gas: quantity(U256::from(2)),
                max_priority_fee_per_gas: quantity(U256::from(1)),
                rollup_fee: None,
            },
            resource: resource(1, owner()),
            distributor: DISTRIBUTOR.into(),
            root: format!("{root:#x}"),
            claims: vec![claim],
            payout_semantics: Some(
                ClaimPayoutSemantics::CumulativeRemainderAtExecutionNoGuaranteedMinimum,
            ),
            value_bounds: None,
            boundary_authorization: None,
            financial_plan: None,
        }
    }

    fn event_log<E: SolEvent>(event: &E, emitter: Address) -> Value {
        let encoded = event.encode_log_data();
        json!({"address":lower_address(emitter),"topics":encoded.topics().iter().map(|t|format!("{t:#x}")).collect::<Vec<_>>(),"data":format!("0x{}",hex::encode(encoded.data)),"transactionHash":"0x01","blockHash":"0x02","blockNumber":"0x3","removed":false})
    }

    fn successful_receipt() -> Value {
        json!({"transactionHash":"0x01","blockHash":"0x02","blockNumber":"0x3","logs":[
            event_log(&MerklDistributor::Claimed{user:owner(),token:token(),amount:U256::from(103)},address(DISTRIBUTOR).unwrap()),
            event_log(&crate::abi::Erc20::Transfer{from:address(DISTRIBUTOR).unwrap(),to:owner(),value:U256::from(103)},token())
        ]})
    }

    #[test]
    fn merkl_claim_encodes_cumulative_not_incremental_and_no_approval_or_callback() {
        let b = binding();
        let call = MerklDistributor::claimCall::abi_decode_validate(
            &rpc_bytes(&json!(b.transaction.calldata)).unwrap(),
        )
        .unwrap();
        assert_eq!(call.users, vec![owner()]);
        assert_eq!(call.tokens, vec![token()]);
        assert_eq!(call.amounts, vec![U256::from(123)]);
        assert_eq!(call.proofs, vec![Vec::<B256>::new()]);
        assert_eq!(b.claims[0].payout, quantity(U256::from(103)));
    }

    #[test]
    fn merkl_settlement_requires_matching_claim_and_exact_wallet_transfer() {
        let b = binding();
        let receipt = successful_receipt();
        assert!(verify_claim_events(&receipt, &b).is_ok());
        let mut missing = receipt.clone();
        missing["logs"].as_array_mut().unwrap().pop();
        assert!(verify_claim_events(&missing, &b).is_err());
        let mut redirected = receipt.clone();
        redirected["logs"][1] = event_log(
            &crate::abi::Erc20::Transfer {
                from: address(DISTRIBUTOR).unwrap(),
                to: token(),
                value: U256::from(103),
            },
            token(),
        );
        assert!(verify_claim_events(&redirected, &b).is_err());
        let mut fee_token = receipt.clone();
        fee_token["logs"][1] = event_log(
            &crate::abi::Erc20::Transfer {
                from: address(DISTRIBUTOR).unwrap(),
                to: owner(),
                value: U256::from(102),
            },
            token(),
        );
        assert!(verify_claim_events(&fee_token, &b).is_err());
        let mut cumulative_event = receipt;
        cumulative_event["logs"][0] = event_log(
            &MerklDistributor::Claimed {
                user: owner(),
                token: token(),
                amount: U256::from(123),
            },
            address(DISTRIBUTOR).unwrap(),
        );
        assert!(verify_claim_events(&cumulative_event, &b).is_err());
    }

    #[test]
    fn merkl_settlement_rejects_unexpected_duplicate_and_noncanonical_events() {
        let b = binding();
        let receipt = successful_receipt();
        let mut duplicate = receipt.clone();
        duplicate["logs"]
            .as_array_mut()
            .unwrap()
            .push(receipt["logs"][0].clone());
        assert!(verify_claim_events(&duplicate, &b).is_err());
        let mut removed = receipt.clone();
        removed["logs"][0]["removed"] = json!(true);
        assert!(verify_claim_events(&removed, &b).is_err());
        let mut wrong_tx = receipt.clone();
        wrong_tx["logs"][1]["transactionHash"] = json!("0x99");
        assert!(verify_claim_events(&wrong_tx, &b).is_err());
        let mut unexpected = receipt;
        unexpected["logs"][0] = event_log(
            &MerklDistributor::Claimed {
                user: owner(),
                token: owner(),
                amount: U256::from(103),
            },
            address(DISTRIBUTOR).unwrap(),
        );
        assert!(verify_claim_events(&unexpected, &b).is_err());
    }

    fn synthetic_adapter() -> EthereumAdapter {
        EthereumAdapter::new(
            EthereumConfig {
                chain_id: 1,
                ..Default::default()
            },
            vec![WalletConnection {
                name: "synthetic".into(),
                address: lower_address(owner()),
                signing_enabled: true,
            }],
            Arc::new(EvmRuntime::new(
                Arc::new(crate::tests::Fixture::new()),
                1,
                None,
            )),
        )
        .unwrap()
    }

    #[test]
    fn merkl_network_aliases_preserve_exact_chain_identity() {
        let adapter = synthetic_adapter();
        assert!(adapter.reward_network(1, "ethereum").is_ok());
        assert!(adapter.reward_network(1, "evm").is_ok());
        assert!(adapter.reward_network(8453, "evm").is_err());
        assert!(adapter.reward_network(1, "base").is_err());
        assert!(adapter.reward_network(10, "evm").is_err());
    }

    #[test]
    fn merkl_api_enumeration_never_claims_complete_wallet_coverage() {
        let selected = selected_tokens(&[lower_address(token())]).unwrap();
        assert_eq!(reward_coverage(None, &[], true)["complete"], false);
        assert_eq!(
            reward_coverage(Some(&selected), &[], true)["complete"],
            true
        );
        assert_eq!(
            reward_coverage(Some(&selected), &[], false)["complete"],
            false
        );
        let missing = vec![lower_address(token())];
        let coverage = reward_coverage(Some(&selected), &missing, true);
        assert_eq!(coverage["complete"], false);
        assert_eq!(coverage["missing_selected_tokens"], json!(missing));
        assert_eq!(coverage["wallet_enumeration_complete"], false);
    }

    struct ChangingPrices {
        calls: std::sync::Mutex<Vec<(AssetId, Quantity)>>,
    }
    #[async_trait]
    impl WalletValuation for ChangingPrices {
        async fn quote_usd(&self, _: &AssetId, _: Quantity) -> Result<(Quantity, String)> {
            panic!("reward valuation must retain provider observation time")
        }
        async fn quote_observation(
            &self,
            asset: &AssetId,
            units: Quantity,
        ) -> Result<(Quantity, String, String)> {
            let mut calls = self.calls.lock().unwrap();
            calls.push((asset.clone(), units.clone()));
            let price = Quantity::new(if calls.len() == 1 {
                "3000000000"
            } else {
                "2500000"
            })?;
            let timestamp = chrono::Utc::now() - chrono::Duration::seconds(calls.len() as i64 * 10);
            Ok((
                units.mul_div_ceil(&price, &quantity(pow10(asset.decimals)?))?,
                "changing synthetic price".into(),
                timestamp.to_rfc3339(),
            ))
        }
    }
    #[tokio::test]
    async fn merkl_valuation_uses_each_recorded_price_once_and_retains_oldest_time() {
        let prices = Arc::new(ChangingPrices {
            calls: std::sync::Mutex::new(vec![]),
        });
        let adapter = synthetic_adapter().with_wallet_valuation(prices.clone());
        let binding = binding();
        let valuation = adapter
            .merkl_claim_valuation(&binding.transaction, &binding.claims)
            .await
            .unwrap();
        assert_eq!(valuation.prices.len(), 2);
        let native = &valuation.prices[0];
        let reward = &valuation.prices[1];
        assert_eq!(
            valuation.maximum_fee_usd_micros,
            quantity(binding.transaction.maximum_cost().unwrap())
                .mul_div_ceil(&native.usd_micros_per_unit, &native.unit_atoms)
                .unwrap()
        );
        assert_eq!(valuation.movement_usd_micros, Quantity::new("258").unwrap());
        assert_eq!(valuation.observed_at, reward.observed_at);
        let calls = prices.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0], (native.asset.clone(), native.unit_atoms.clone()));
        assert_eq!(calls[1], (reward.asset.clone(), reward.unit_atoms.clone()));
    }

    fn receipt_with_payouts(claims: &[ClaimToken], payouts: &[u64]) -> Value {
        let mut receipt = successful_receipt();
        let mut logs = Vec::new();
        for (claim, payout) in claims.iter().zip(payouts) {
            let token = address(&claim.token.reference).unwrap();
            logs.push(event_log(
                &MerklDistributor::Claimed {
                    user: owner(),
                    token,
                    amount: U256::from(*payout),
                },
                address(DISTRIBUTOR).unwrap(),
            ));
            if *payout > 0 {
                logs.push(event_log(
                    &crate::abi::Erc20::Transfer {
                        from: address(DISTRIBUTOR).unwrap(),
                        to: owner(),
                        value: U256::from(*payout),
                    },
                    token,
                ));
            }
        }
        receipt["logs"] = json!(logs);
        receipt
    }

    #[test]
    fn merkl_concurrent_claims_settle_actual_partial_zero_and_mixed_payouts() {
        let mut b = binding();
        for payout in [0, 42, 103] {
            let receipt = receipt_with_payouts(&b.claims, &[payout]);
            let actual = verify_claim_events(&receipt, &b).unwrap();
            assert_eq!(
                actual[&b.claims[0].token.reference],
                Quantity::new(payout.to_string()).unwrap()
            );
        }
        let mut second = b.claims[0].clone();
        second.token.reference = lower_address(Address::repeat_byte(0x33));
        b.claims.push(second);
        let actual = verify_claim_events(&receipt_with_payouts(&b.claims, &[0, 42]), &b).unwrap();
        assert_eq!(actual[&b.claims[0].token.reference], Quantity::zero());
        assert_eq!(
            actual[&b.claims[1].token.reference],
            Quantity::new("42").unwrap()
        );

        let excessive = receipt_with_payouts(&b.claims, &[0, 104]);
        assert!(verify_claim_events(&excessive, &b).is_err());
        let mut conflicting = receipt_with_payouts(&b.claims, &[0, 42]);
        conflicting["logs"].as_array_mut().unwrap().push(event_log(
            &crate::abi::Erc20::Transfer {
                from: address(DISTRIBUTOR).unwrap(),
                to: owner(),
                value: U256::from(1),
            },
            token(),
        ));
        assert!(
            verify_claim_events(&conflicting, &b).is_err(),
            "zero payout cannot hide a transfer"
        );
        let mut conflicting = receipt_with_payouts(&b.claims, &[0, 42]);
        conflicting["logs"][2] = event_log(
            &crate::abi::Erc20::Transfer {
                from: address(DISTRIBUTOR).unwrap(),
                to: owner(),
                value: U256::from(41),
            },
            address(&b.claims[1].token.reference).unwrap(),
        );
        assert!(
            verify_claim_events(&conflicting, &b).is_err(),
            "partial payout still requires its exact transfer"
        );
    }

    #[test]
    fn merkl_legacy_bindings_keep_their_exact_payout_authority() {
        let mut legacy = binding();
        legacy.version = 1;
        legacy.payout_semantics = None;
        let mut prepared = prepared_claim();
        prepared.binding = serde_json::to_value(&legacy).unwrap();
        synthetic_adapter().verify_claim_binding(&prepared).unwrap();
        assert!(
            verify_claim_events(&receipt_with_payouts(&legacy.claims, &[103]), &legacy).is_ok()
        );
        for payout in [0, 42] {
            assert!(
                verify_claim_events(&receipt_with_payouts(&legacy.claims, &[payout]), &legacy)
                    .is_err()
            );
        }
        prepared.binding["version"] = json!(2);
        assert!(synthetic_adapter().verify_claim_binding(&prepared).is_err());
    }

    // Only dispatch is controlled: real preparation, intent, protected engine
    // authorization/journal/reservations and real canonical receipt reconciliation
    // are used, without signing or sending any network transaction.
    struct ConcurrentClaimAdapter {
        inner: EthereumAdapter,
        rpc: Arc<RewardRpc>,
        dispatches: AtomicUsize,
    }
    #[async_trait]
    impl Adapter for ConcurrentClaimAdapter {
        fn definitions(&self) -> Vec<Definition> {
            super::super::definitions()
                .into_iter()
                .filter(|d| d.name == CLAIM_OPERATION)
                .collect()
        }
        fn connections(&self) -> Vec<ConnectionMetadata> {
            self.inner.connections()
        }
        async fn snapshot(&self) -> Result<StateSnapshot> {
            Ok(StateSnapshot {
                state: gofin_domain::FinancialState {
                    holdings: vec![ResourceAmount {
                        resource: resource(1, owner()),
                        amount: amount(&self.inner.native_asset(), pow10(18)?),
                    }],
                    ..Default::default()
                },
                observed_at: now(),
                complete: true,
                coverage: vec![resource(1, owner()).0],
                pending: vec![],
            })
        }
        async fn snapshot_scoped(&self, _: &gofin_domain::SnapshotScope) -> Result<StateSnapshot> {
            self.snapshot().await
        }
        async fn read(&self, _: &str, _: &Value) -> Result<Value> {
            Err(GoFinError::validation(
                "Controlled claim test has no reads.",
            ))
        }
        async fn prepare(&self, _: &str, input: &Value) -> Result<PreparedOperation> {
            self.inner
                .morpho_claim_prepare(input, &PreparationContext::default())
                .await
        }
        async fn revalidate(&self, prepared: &PreparedOperation) -> Result<PreparedOperation> {
            self.inner.morpho_claim_revalidate(prepared).await
        }
        async fn submit(
            &self,
            prepared: &PreparedOperation,
            journal: Arc<dyn SubmissionJournal>,
        ) -> Result<Submission> {
            journal.assert_candidate(prepared)?;
            let b = self.inner.verify_claim_binding(prepared)?;
            let submission = Submission {
                kind: "ethereum".into(),
                reference: format!("{:#x}", B256::repeat_byte(0xaa)),
                data: json!({"transaction":b.transaction,"metadata":{"operation":CLAIM_OPERATION,"resource":b.resource,"distributor":b.distributor,"root":b.root}}),
            };
            journal.persist_submission(&submission)?;
            *self.rpc.transaction.lock().unwrap() = Some(b.transaction);
            self.dispatches.fetch_add(1, Ordering::SeqCst);
            Ok(submission)
        }
        async fn reconcile(&self, record: &OperationRecord) -> Result<Reconciliation> {
            self.inner.morpho_claim_reconcile(record).await
        }
    }
    struct ClaimPrices;
    #[async_trait]
    impl WalletValuation for ClaimPrices {
        async fn quote_usd(&self, asset: &AssetId, units: Quantity) -> Result<(Quantity, String)> {
            let price = Quantity::new(if asset.reference == "native" {
                "3000000000"
            } else {
                "2500000"
            })?;
            Ok((
                units.mul_div_ceil(&price, &quantity(pow10(asset.decimals)?))?,
                "fixed synthetic reward price".into(),
            ))
        }
    }
    struct ClaimApproval;
    #[async_trait]
    impl gofin_domain::Authorizer for ClaimApproval {
        async fn authorize(
            &self,
            challenge: &gofin_domain::AuthorizationChallenge,
        ) -> Result<gofin_domain::ApprovalAudit> {
            Ok(gofin_domain::ApprovalAudit {
                source: None,
                yolo_provenance: None,
                mechanism: "synthetic-concurrent-claim-approval".into(),
                binding_hash: challenge.binding_hash.clone(),
                challenge_nonce: challenge.nonce.clone(),
                approved_at: now(),
                expires_at: challenge.expires_at.clone(),
            })
        }
    }

    #[tokio::test]
    async fn merkl_concurrent_claim_engine_clears_reservations_without_replaying_after_restart() {
        use gofin_engine::{Engine, EngineOptions};
        use gofin_store::Store;
        for payout in [0, 42] {
            let rpc = Arc::new(RewardRpc::new());
            let inner = rpc_adapter(rpc.clone(), api_response())
                .with_wallet_valuation(Arc::new(ClaimPrices));
            let adapter = Arc::new(ConcurrentClaimAdapter {
                inner,
                rpc: rpc.clone(),
                dispatches: AtomicUsize::new(0),
            });
            let directory = tempfile::tempdir().unwrap();
            let database = directory.path().join("concurrent-reward-claim.sqlite");
            let store = Store::open(&database).unwrap().share();
            let engine = Engine::new_with_shared_store(
                store.clone(),
                vec![adapter.clone()],
                Arc::new(ClaimApproval),
                EngineOptions::default(),
            )
            .unwrap();
            let request = gofin_domain::OperationRequest {
                request_id: "concurrent-claim".into(),
                operation: CLAIM_OPERATION.into(),
                input: prepared_claim().input,
                run_id: None,
            };
            let first = engine.execute(request.clone()).await.unwrap();
            assert!(first.submission.is_some(), "{first:?}");
            assert!(
                !first.status.terminal(),
                "receipt has not arrived: {first:?}"
            );
            assert!(store.lock().unwrap().reservation_count(&first.id).unwrap() > 0);
            let preview = first.preview.as_ref().unwrap();
            assert!(preview.summary.contains("no guaranteed minimum"));
            assert_eq!(preview.binding["version"], 2);
            assert_eq!(
                preview.binding["payout_semantics"],
                "cumulative_remainder_at_execution_no_guaranteed_minimum"
            );
            assert!(preview.intent.expected.iter().all(|e| matches!(e, EffectConstraint::Credit { minimum_credit, .. } if minimum_credit.is_zero())));
            drop(engine);
            drop(store);
            rpc.claimed.store(123, Ordering::SeqCst);
            rpc.receipt_payout.store(payout, Ordering::SeqCst);
            rpc.settled.store(true, Ordering::SeqCst);
            let store = Store::open(&database).unwrap().share();
            let restarted = Engine::new_with_shared_store(
                store.clone(),
                vec![adapter.clone()],
                Arc::new(ClaimApproval),
                EngineOptions::default(),
            )
            .unwrap();
            let settled = restarted.status(&first.id, true).await.unwrap();
            assert_eq!(settled.status, Status::Completed, "{settled:?}");
            assert_eq!(
                store.lock().unwrap().reservation_count(&first.id).unwrap(),
                0
            );
            let verified = settled.effects.as_ref().unwrap();
            let credits = verified
                .effects
                .iter()
                .filter_map(|effect| match effect {
                    FinancialEffect::UsableCredit { amount, .. } => Some(amount.atoms.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(
                credits,
                if payout == 0 {
                    vec![]
                } else {
                    vec![Quantity::new(payout.to_string()).unwrap()]
                }
            );
            assert_eq!(
                verified.evidence["actual_payouts"][lower_address(token())],
                payout.to_string()
            );
            assert_eq!(restarted.execute(request).await.unwrap().id, first.id);
            assert_eq!(adapter.dispatches.load(Ordering::SeqCst), 1);
        }
    }

    fn prepared_claim() -> PreparedOperation {
        PreparedOperation {
            action_deadline: None,
            operation: CLAIM_OPERATION.into(),
            input: json!({"wallet":"synthetic","network":"ethereum","chain_id":1,"tokens":[lower_address(token())],"request_id":"reward-1","max_fee_eth":"0.0001"}),
            binding: serde_json::to_value(binding()).unwrap(),
            summary: "fixture".into(),
            checks: vec![],
            warnings: vec![],
            observed_at: now(),
            expires_at: (chrono::Utc::now() + chrono::Duration::seconds(120)).to_rfc3339(),
            valuation: None,
            intent: FinancialIntent::default(),
            operation_version: 1,
        }
    }

    #[test]
    fn merkl_binding_freezes_owner_chain_tokens_cumulative_proof_and_calldata() {
        let adapter = synthetic_adapter();
        let prepared = prepared_claim();
        adapter.verify_claim_binding(&prepared).unwrap();
        let mut changed = prepared.clone();
        changed.binding["claims"][0]["payout"] = json!("104");
        assert!(adapter.verify_claim_binding(&changed).is_err());
        let mut changed = prepared.clone();
        changed.binding["transaction"]["sender"] = json!(lower_address(token()));
        assert!(adapter.verify_claim_binding(&changed).is_err());
        let mut changed = prepared.clone();
        changed.binding["transaction"]["calldata"] = json!("0xdeadbeef");
        assert!(adapter.verify_claim_binding(&changed).is_err());
        let mut changed = prepared.clone();
        changed.input["tokens"] = json!([lower_address(owner())]);
        assert!(adapter.verify_claim_binding(&changed).is_err());
        let mut changed = prepared.clone();
        changed.binding["unrecognized_authority"] = json!(true);
        assert!(adapter.verify_claim_binding(&changed).is_err());
        let mut changed = prepared;
        changed.binding["transaction"]["value"] = json!("1");
        assert!(adapter.verify_claim_binding(&changed).is_err());
    }

    #[test]
    fn merkl_plan_candidate_respects_finite_payout_and_fee_caps() {
        let adapter = synthetic_adapter();
        let prepared = prepared_claim();
        let b = binding();
        let mut spec = BoundarySpec {
            fixed: json!({"operation":CLAIM_OPERATION,"input":claim_fixed_input(&prepared.input),"signer":lower_address(owner()),"distributor":DISTRIBUTOR,"tokens":b.claims.iter().map(|c|&c.token).collect::<Vec<_>>(),"payout_semantics":b.payout_semantics}),
            parameters: json!({"maximums":{lower_address(token()):"103"},"max_fee_eth":"0.0001"}),
            maximum_debits: vec![],
            maximum_costs: vec![],
            summary: "fixture".into(),
        };
        assert!(
            adapter
                .morpho_claim_validate_candidate(&spec, &prepared)
                .is_ok()
        );
        let mut legacy_spec = spec.clone();
        legacy_spec
            .fixed
            .as_object_mut()
            .unwrap()
            .remove("payout_semantics");
        assert_eq!(
            adapter
                .morpho_claim_validate_candidate(&legacy_spec, &prepared)
                .unwrap_err()
                .code,
            "BOUNDARY_VIOLATION",
            "an old exact-payout named plan cannot authorize new zero-floor semantics"
        );
        spec.parameters["maximums"][lower_address(token())] = json!("102");
        assert_eq!(
            adapter
                .morpho_claim_validate_candidate(&spec, &prepared)
                .unwrap_err()
                .code,
            "BOUNDARY_VIOLATION"
        );
        spec.parameters["maximums"][lower_address(token())] = json!("103");
        spec.parameters["max_fee_eth"] = json!("0.00001");
        assert!(
            adapter
                .morpho_claim_validate_candidate(&spec, &prepared)
                .is_err()
        );
    }
}
