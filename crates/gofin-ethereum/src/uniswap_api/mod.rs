//! Hosted Uniswap swaps. Financial metadata is independently bounded; execution
//! payload semantics are trusted to the fixed provider, as with Jupiter spot.
pub mod api;
pub(crate) mod permissions;
mod settlement;
#[cfg(test)]
pub(crate) mod tests;

use super::*;
use api::{QuoteIntent, RoutingPreference, ValidatedQuote};
use gofin_evm::{
    ProviderTransactionRequest, TrustedUniswapTypedData, rpc_bytes, rpc_u64, rpc_u256,
};

pub(crate) const PROVIDER: &str = "uniswap_api_v1";
pub(crate) const ENDPOINT: &str = api::BASE_URL;
const PERMIT2: &str = "0x000000000022d473030f116ddee9f6b43ac78ba3";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct UniswapBinding {
    pub provider: String,
    pub endpoint: String,
    pub chain_id: u64,
    pub owner: String,
    pub resource: ResourceId,
    pub token: AssetId,
    pub output: AssetId,
    pub units: Quantity,
    pub minimum_output: Quantity,
    pub quote_output: Quantity,
    pub maximum_fee: Quantity,
    pub deadline: u64,
    pub route_preference: String,
    pub price_impact_bps: u32,
    pub quote: ValidatedQuote,
    pub transaction: Option<FrozenTransaction>,
    pub prepared_block: u64,
    pub dex: String,
    #[serde(flatten)]
    pub metadata: BTreeMap<String, Value>,
}
pub(crate) fn is_api(prepared: &PreparedOperation) -> bool {
    prepared.binding["provider"] == PROVIDER
}
pub(crate) fn requested(operation: &str, input: &Value) -> bool {
    operation == "uniswap.swap" || (operation == "swap.execute" && input["dex"] == "uniswap")
}
pub(crate) fn parse_binding(prepared: &PreparedOperation) -> Result<UniswapBinding> {
    let b: UniswapBinding = serde_json::from_value(prepared.binding.clone())
        .map_err(|_| GoFinError::validation("Invalid original Uniswap API binding."))?;
    require(
        b.provider == PROVIDER && b.endpoint == ENDPOINT && b.dex == "uniswap",
        "BINDING_INVALID",
        "Unknown Uniswap provider binding.",
    )?;
    Ok(b)
}
fn token_address(asset: &AssetId) -> String {
    if asset.reference == "native" {
        lower_address(Address::ZERO)
    } else {
        asset.reference.clone()
    }
}
fn preference(value: Option<&str>) -> Result<RoutingPreference> {
    match value.unwrap_or("auto") {
        "auto" => Ok(RoutingPreference::Automatic),
        "amm" => Ok(RoutingPreference::Amm),
        "uniswapx" => Ok(RoutingPreference::Uniswapx),
        _ => Err(GoFinError::validation(
            "Select auto, amm or uniswapx routing.",
        )),
    }
}
fn deadline_at(adapter: &EthereumAdapter, requested: Option<u64>) -> Result<u64> {
    let now = adapter.clock.now().timestamp().max(0) as u64;
    let deadline = requested.unwrap_or(now + 600);
    require(
        deadline > now && deadline <= now + 600,
        "DEADLINE_INVALID",
        "Swap deadline must be current and within 600 seconds.",
    )?;
    Ok(deadline)
}
fn quote_intent(b: &UniswapBinding, input: &Value) -> Result<QuoteIntent> {
    Ok(QuoteIntent {
        chain_id: b.chain_id,
        wallet: b.owner.clone(),
        token_in: token_address(&b.token),
        token_out: token_address(&b.output),
        amount_in: b.units.to_string(),
        minimum_out: b.minimum_output.to_string(),
        slippage_bps: input["slippage_bps"].as_u64().unwrap_or(50) as u16,
        max_price_impact_bps: input["max_price_impact_bps"].as_u64().unwrap_or(10_000) as u32,
        maximum_fee: b.maximum_fee.to_string(),
        maximum_deadline: b.deadline,
        routing: preference(Some(&b.route_preference))?,
    })
}

impl EthereumAdapter {
    pub(crate) async fn uniswap_api_quote(&self, input: &Value) -> Result<Value> {
        let r = request::parse("uniswap.quote", input)?;
        let wallet = self.resolve(&r, false)?;
        self.runtime.assert_chain().await?;
        let (token, output) = tokio::try_join!(
            self.token(r.token_in.as_deref().expect("schema"), None),
            self.token(r.token_out.as_deref().expect("schema"), None)
        )?;
        require(
            token.reference != output.reference,
            "SWAP_ASSET_INVALID",
            "Swap tokens must differ.",
        )?;
        let amount = Quantity::from_decimal(r.amount.as_deref().expect("schema"), token.decimals)?;
        let intent = QuoteIntent {
            chain_id: self.config.chain_id,
            wallet: wallet.address.clone(),
            token_in: token_address(&token.asset(self)),
            token_out: token_address(&output.asset(self)),
            amount_in: amount.to_string(),
            minimum_out: "1".into(),
            slippage_bps: r.slippage_bps.unwrap_or(50),
            max_price_impact_bps: r.max_price_impact_bps.unwrap_or(10_000),
            maximum_fee: Quantity::from_decimal(r.max_fee_eth.as_deref().unwrap_or("1"), 18)?
                .to_string(),
            maximum_deadline: deadline_at(self, r.deadline)?,
            routing: preference(r.route_preference.as_deref())?,
        };
        let q = self.uniswap_api.quote(&intent).await?;
        Ok(
            json!({"provider":PROVIDER,"routing":q.routing,"quote_id":q.quote_id,
            "input":{"asset":token.asset(self),"amount":amount.to_decimal(token.decimals),"atoms":amount},
            "output":{"asset":output.asset(self),"expected":Quantity::new(&q.amount_out)?.to_decimal(output.decimals),"minimum":Quantity::new(&q.minimum_out)?.to_decimal(output.decimals)},
            "gas_fee_wei":q.gas_fee,"price_impact_bps":q.price_impact_bps,"deadline":q.deadline,
            "quote":q.raw["quote"],"permission":"ERC-20 inputs require a finite allowance to canonical Permit2; sign any returned permit only after financial authorization."}),
        )
    }

    pub(crate) async fn uniswap_api_prepare(
        &self,
        operation: &str,
        input: &Value,
    ) -> Result<PreparedOperation> {
        let r = request::parse(operation, input)?;
        let w = self.resolve(&r, true)?;
        let owner = address(&w.address)?;
        require(
            r.recipient
                .as_deref()
                .is_none_or(|v| address(v).is_ok_and(|a| a == owner)),
            "RECIPIENT_INVALID",
            "Uniswap output must return to the signing wallet.",
        )?;
        self.runtime.assert_chain().await?;
        let (token, output) = tokio::try_join!(
            self.token(r.token_in.as_deref().expect("schema"), None),
            self.token(r.token_out.as_deref().expect("schema"), None)
        )?;
        require(
            token.reference != output.reference,
            "SWAP_ASSET_INVALID",
            "Swap tokens must differ.",
        )?;
        let deadline = deadline_at(self, r.deadline)?;
        let maximum_fee = Quantity::from_decimal(r.max_fee_eth.as_deref().expect("schema"), 18)?;
        let balance = self.balance_of(&token, owner, None).await?;
        let units = if r.amount.as_deref() == Some("all") {
            let cap = exact(r.max_amount.as_deref().expect("schema"), token.decimals)?;
            let available = if token.reference == "native" {
                balance.saturating_sub(u256(&maximum_fee)?)
            } else {
                balance
            };
            require(
                available > U256::ZERO && available <= cap && cap < U256::MAX,
                "SWEEP_MAXIMUM",
                "Complete available balance must fit finite max_amount.",
            )?;
            quantity(available)
        } else {
            Quantity::from_decimal(r.amount.as_deref().expect("schema"), token.decimals)?
        };
        require(
            !units.is_zero() && u256(&units)? <= balance,
            "INSUFFICIENT_BALANCE",
            "Wallet cannot fund the exact positive swap input.",
        )?;
        let minimum_output =
            Quantity::from_decimal(r.min_output.as_deref().expect("schema"), output.decimals)?;
        let intent = QuoteIntent {
            chain_id: self.config.chain_id,
            wallet: lower_address(owner),
            token_in: token_address(&token.asset(self)),
            token_out: token_address(&output.asset(self)),
            amount_in: units.to_string(),
            minimum_out: minimum_output.to_string(),
            slippage_bps: r.slippage_bps.expect("schema"),
            max_price_impact_bps: r.max_price_impact_bps.unwrap_or(10_000),
            maximum_fee: maximum_fee.to_string(),
            maximum_deadline: deadline,
            routing: preference(r.route_preference.as_deref())?,
        };
        self.uniswap_api_allowance(&token.asset(self), owner, &units)
            .await?;
        let quote = self.uniswap_api.quote(&intent).await?;
        let effective_deadline = quote.deadline.unwrap_or(deadline).min(deadline);
        let actual_fee = if quote.routing == "CLASSIC" {
            maximum_fee
        } else {
            Quantity::zero()
        };
        let mut assets = vec![token.asset(self), output.asset(self)];
        if !assets.contains(&self.native_asset()) {
            assets.push(self.native_asset());
        }
        let amounts = assets
            .into_iter()
            .map(|asset| {
                Ok(Amount {
                    atoms: quantity(pow10(asset.decimals)?),
                    asset,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let prices: Vec<_> = self
            .quote_verified_assets_usd_observed(&amounts)
            .await?
            .into_iter()
            .zip(amounts)
            .map(|((usd, source, observed_at), amount)| AssetPrice {
                asset: amount.asset,
                unit_atoms: amount.atoms,
                usd_micros_per_unit: usd,
                source,
                observed_at,
            })
            .collect();
        let usd = |asset: &AssetId, units: &Quantity| -> Result<Quantity> {
            let p = prices
                .iter()
                .find(|p| p.asset == *asset)
                .ok_or_else(|| GoFinError::validation("Missing swap valuation."))?;
            units.mul_div_ceil(&p.usd_micros_per_unit, &p.unit_atoms)
        };
        let movement = usd(&token.asset(self), &units)?;
        let output_value = usd(&output.asset(self), &Quantity::new(&quote.amount_out)?)?;
        let price_impact_bps = match quote.price_impact_bps {
            Some(bps) => bps,
            None if movement > output_value => movement
                .checked_sub(&output_value)?
                .mul_div_ceil(&Quantity::from_u64(10_000), &movement)?
                .to_string()
                .parse::<u32>()
                .map_err(|_| GoFinError::validation("Swap price impact overflows."))?,
            None => 0,
        };
        require(
            price_impact_bps <= r.max_price_impact_bps.unwrap_or(10_000),
            "PRICE_IMPACT_LIMIT",
            "Quoted output value exceeds the approved price-impact bound.",
        )?;
        let fee_usd = usd(&self.native_asset(), &actual_fee)?;
        let observed = self.clock.now().to_rfc3339();
        let price_time = prices
            .iter()
            .map(|p| p.observed_at.clone())
            .min()
            .ok_or_else(|| GoFinError::validation("Missing prices."))?;
        let resource = resource(self.config.chain_id, owner);
        let b = UniswapBinding {
            provider: PROVIDER.into(),
            endpoint: ENDPOINT.into(),
            chain_id: self.config.chain_id,
            owner: lower_address(owner),
            resource: resource.clone(),
            token: token.asset(self),
            output: output.asset(self),
            units: units.clone(),
            minimum_output: Quantity::new(&quote.minimum_out)?,
            quote_output: Quantity::new(&quote.amount_out)?,
            maximum_fee: actual_fee.clone(),
            deadline: effective_deadline,
            route_preference: r.route_preference.clone().unwrap_or_else(|| "auto".into()),
            price_impact_bps,
            quote,
            transaction: None,
            prepared_block: rpc_u64(
                &self
                    .runtime
                    .rpc
                    .request("eth_blockNumber", json!([]))
                    .await?,
            )?,
            dex: "uniswap".into(),
            metadata: BTreeMap::new(),
        };
        // All slow preparation finishes before the short executable quote window
        // is checked; none of it grants or consumes human authority.
        require(
            b.deadline > self.clock.now().timestamp().max(0) as u64,
            "PREVIEW_EXPIRED",
            "Uniswap quote expired during preparation.",
        )?;
        let expires = chrono::DateTime::from_timestamp(b.deadline as i64, 0)
            .ok_or_else(|| GoFinError::validation("Invalid quote expiry."))?
            .min(self.clock.now() + chrono::Duration::seconds(60))
            .to_rfc3339();
        let prepared=PreparedOperation {operation:operation.into(),input:input.clone(),binding:serde_json::to_value(&b).map_err(|_|GoFinError::validation("Invalid binding."))?,summary:format!("Swap {} {} for at least {} {} via {}",units.to_decimal(token.decimals),token.symbol,b.minimum_output.to_decimal(output.decimals),output.symbol,b.quote.routing),checks:vec![Check{name:"Uniswap quote financial bounds".into(),status:CheckStatus::Passed,detail:"Exact wallet, chain, assets, input, net minimum output, price impact, slippage and fee metadata match the approved scope. Provider execution payload semantics are trusted.".into()}],warnings:vec!["Uniswap and the GoFin proxy construct the execution payload. An accepted submission is not settlement; reconcile uncertain outcomes without replay.".into()],observed_at:observed,expires_at:expires,valuation:Some(Valuation {movement_usd_micros:movement,maximum_fee_usd_micros:fee_usd,additional_allocation_usd_micros:Quantity::zero(),observed_at:price_time,source:prices.iter().find(|p|p.asset==b.token).expect("input price").source.clone(),prices}),intent:FinancialIntent {debits:vec![ResourceAmount{resource:resource.clone(),amount:Amount{asset:b.token.clone(),atoms:units.clone()}}],maximum_costs:vec![CostBound{location:AccountingLocation::Cash{resource:resource.clone()},amount:Amount{asset:self.native_asset(),atoms:actual_fee},treatment:CostTreatment::AdditionalDebit}],expected:vec![EffectConstraint::Conversion{resource:resource.clone(),input:b.token.clone(),maximum_input:units,output:b.output.clone(),minimum_output:b.minimum_output.clone()}],locks:vec![resource]},operation_version:2,action_deadline:None};
        Ok(prepared)
    }

    async fn uniswap_api_allowance(
        &self,
        token: &AssetId,
        owner: Address,
        units: &Quantity,
    ) -> Result<()> {
        if token.reference == "native" {
            return Ok(());
        }
        let t = reads::Token {
            reference: token.reference.clone(),
            decimals: token.decimals,
            symbol: token.reference.clone(),
        };
        let allowance = self.allowance(&t, owner, address(PERMIT2)?, None).await?;
        require(allowance>=u256(units)? && allowance<U256::MAX,"ALLOWANCE_INSUFFICIENT","Approve a finite allowance for the input token to canonical Permit2 before preparing this swap.").map_err(|e|e.with_details(json!({"operation":"wallet.approve","token":token.reference,"spender":PERMIT2,"amount":units.to_decimal(token.decimals)})))
    }

    pub(crate) fn uniswap_api_revalidate<'a>(
        &'a self,
        prepared: &'a PreparedOperation,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<PreparedOperation>> + Send + 'a>>
    {
        // Construct and heap-pin outside the shared async dispatcher. Boxing
        // only at its await site still reserves the large construction temporary
        // on every EVM revalidation poll, including unrelated Aave operations.
        Box::pin(self.uniswap_api_revalidate_inner(prepared))
    }

    async fn uniswap_api_revalidate_inner(
        &self,
        prepared: &PreparedOperation,
    ) -> Result<PreparedOperation> {
        let b = parse_binding(prepared)?;
        require(
            b.chain_id == self.config.chain_id && requested(&prepared.operation, &prepared.input),
            "BINDING_INVALID",
            "Uniswap provider binding belongs to another operation or chain.",
        )?;
        let r = request::parse(&prepared.operation, &prepared.input)?;
        let w = self.resolve(&r, true)?;
        require(
            address(&w.address)? == address(&b.owner)?
                && b.resource == resource(b.chain_id, address(&b.owner)?),
            "BINDING_INVALID",
            "Approved Uniswap wallet identity changed.",
        )?;
        require(
            chrono::DateTime::parse_from_rfc3339(&prepared.expires_at)
                .is_ok_and(|t| t > self.clock.now())
                && b.deadline > self.clock.now().timestamp().max(0) as u64,
            "PREVIEW_EXPIRED",
            "Original Uniswap quote expired; prepare a fresh candidate within existing authority.",
        )?;
        api::validate_quote(b.quote.raw.clone(), &quote_intent(&b, &prepared.input)?)?;
        self.runtime.assert_chain().await?;
        if r.amount.as_deref() == Some("all") {
            let token = reads::Token {
                reference: b.token.reference.clone(),
                decimals: b.token.decimals,
                symbol: b.token.reference.clone(),
            };
            let balance = self.balance_of(&token, address(&b.owner)?, None).await?;
            let available = if b.token.reference == "native" {
                balance.saturating_sub(u256(&b.maximum_fee)?)
            } else {
                balance
            };
            require(
                available == u256(&b.units)?,
                "BINDING_CHANGED",
                "Complete available swap balance changed; prepare a new bounded candidate.",
            )?;
        }
        self.uniswap_api_allowance(&b.token, address(&b.owner)?, &b.units)
            .await?;
        if let Some(tx) = &b.transaction {
            self.runtime.revalidate_provider_transaction(tx).await?;
        }
        let mut fresh = prepared.clone();
        let old = prepared
            .valuation
            .as_ref()
            .ok_or_else(|| GoFinError::validation("Missing approved swap valuation."))?;
        let amounts = old
            .prices
            .iter()
            .map(|p| Amount {
                asset: p.asset.clone(),
                atoms: p.unit_atoms.clone(),
            })
            .collect::<Vec<_>>();
        let prices = self
            .quote_verified_assets_usd_observed(&amounts)
            .await?
            .into_iter()
            .zip(amounts)
            .map(|((usd, source, observed_at), a)| AssetPrice {
                asset: a.asset,
                unit_atoms: a.atoms,
                usd_micros_per_unit: usd,
                source,
                observed_at,
            })
            .collect::<Vec<_>>();
        let price = |asset: &AssetId| {
            prices
                .iter()
                .find(|p| p.asset == *asset)
                .ok_or_else(|| GoFinError::validation("Missing swap price identity."))
        };
        let input_price = price(&b.token)?;
        let fee_price = price(&self.native_asset())?;
        fresh.valuation = Some(Valuation {
            movement_usd_micros: b
                .units
                .mul_div_ceil(&input_price.usd_micros_per_unit, &input_price.unit_atoms)?,
            maximum_fee_usd_micros: b
                .maximum_fee
                .mul_div_ceil(&fee_price.usd_micros_per_unit, &fee_price.unit_atoms)?,
            additional_allocation_usd_micros: Quantity::zero(),
            observed_at: prices
                .iter()
                .map(|p| p.observed_at.clone())
                .min()
                .ok_or_else(|| GoFinError::validation("Missing swap prices."))?,
            source: input_price.source.clone(),
            prices,
        });
        fresh.observed_at = self.clock.now().to_rfc3339();
        carry_attempt_valuation(prepared, &mut fresh, self.clock.now())?;
        Ok(fresh)
    }

    pub(crate) async fn uniswap_api_submit(
        &self,
        validated: &RevalidatedOperation,
        journal: Arc<dyn SubmissionJournal>,
    ) -> Result<Submission> {
        validated.assert_fresh()?;
        let prepared = validated.prepared();
        let b = parse_binding(prepared)?;
        journal.assert_candidate(prepared)?;
        let signature = if let Some(data) = &b.quote.permit_data {
            let permit = signing_data(&b, data)?;
            Some(gofin_evm::hex_bytes(
                &self
                    .runtime
                    .sign_uniswap_typed_data(&permit, journal.clone())
                    .await?,
            ))
        } else {
            None
        };
        if b.quote.routing != "CLASSIC" {
            let reference = b
                .quote
                .order_hash
                .clone()
                .ok_or_else(|| GoFinError::validation("UniswapX order hash missing."))?;
            let submission = Submission {
                kind: "uniswapx".into(),
                reference,
                data: json!({"provider":PROVIDER,"chain_id":b.chain_id,"owner":b.owner,"quote_id":b.quote.quote_id,"binding_hash":binding_hash(&prepared.binding)?,"deadline":b.deadline}),
            };
            validated.assert_fresh()?;
            journal.assert_candidate(prepared)?;
            journal.persist_submission(&submission)?;
            let response=self.uniswap_api.order(json!({"quote":b.quote.raw["quote"],"routing":b.quote.routing,"signature":signature})).await?;
            require(
                response["orderId"]
                    .as_str()
                    .is_some_and(|s| s.eq_ignore_ascii_case(&submission.reference)),
                "SUBMISSION_IDENTITY_MISMATCH",
                "UniswapX response differs from the durable original order identity.",
            )?;
            journal.persist_dispatch_evidence(&submission,&json!({"provider":PROVIDER,"order_id":submission.reference,"order_status":response["orderStatus"]}))?;
            return Ok(submission);
        }
        let transaction = if let Some(tx) = b.transaction.clone() {
            tx
        } else {
            let mut body = json!({"quote":b.quote.raw["quote"],"deadline":b.deadline,"simulateTransaction":true});
            if let Some(signature) = signature {
                body["signature"] = json!(signature);
                body["permitData"] = b.quote.permit_data.clone().unwrap_or(Value::Null);
            }
            let response = self.uniswap_api.swap(body).await?;
            let tx = &response["swap"];
            require(
                provider_u64(&tx["chainId"])? == b.chain_id
                    && address(tx["from"].as_str().unwrap_or(""))? == address(&b.owner)?,
                "BINDING_INVALID",
                "Provider transaction chain or sender differs from the approved quote.",
            )?;
            let value = provider_uint(&tx["value"])?;
            require(
                value
                    <= if b.token.reference == "native" {
                        u256(&b.units)?
                    } else {
                        U256::ZERO
                    },
                "BINDING_INVALID",
                "Provider transaction native value exceeds the approved input.",
            )?;
            self.runtime
                .prepare_provider_transaction(ProviderTransactionRequest {
                    sender: address(&b.owner)?,
                    to: address(tx["to"].as_str().unwrap_or(""))?,
                    value,
                    calldata: rpc_bytes(&tx["data"])?,
                    gas_limit: provider_u64(&tx["gasLimit"])?,
                    maximum_total_fee: u256(&b.maximum_fee)?,
                })
                .await?
        };
        self.runtime.submit_provider_transaction_revalidated(&transaction,json!({"operation":prepared.operation,"resource":b.resource,"provider":PROVIDER,"binding_hash":binding_hash(&prepared.binding)?}),journal,validated).await
    }
}

fn signing_data(b: &UniswapBinding, data: &Value) -> Result<TrustedUniswapTypedData> {
    require(
        data.is_object()
            && data["types"].is_object()
            && data["values"].is_object()
            && data["domain"].is_object(),
        "UNISWAP_PERMIT_INVALID",
        "Provider permit must contain typed domain, types and values objects.",
    )?;
    let mut typed = data.clone();
    let primary = if b.quote.routing == "CLASSIC" {
        "PermitSingle"
    } else {
        "PermitWitnessTransferFrom"
    };
    typed["primaryType"] = json!(primary);
    typed["message"] = data["values"].clone();
    typed
        .as_object_mut()
        .ok_or_else(|| GoFinError::validation("Invalid provider permit."))?
        .remove("values");
    typed["types"]["EIP712Domain"] = json!([{"name":"name","type":"string"},{"name":"chainId","type":"uint256"},{"name":"verifyingContract","type":"address"}]);
    let expires = provider_u64(
        &typed["message"][if primary == "PermitSingle" {
            "sigDeadline"
        } else {
            "deadline"
        }],
    )?;
    Ok(TrustedUniswapTypedData {
        chain_id: b.chain_id,
        owner: b.owner.clone(),
        expires_at: expires,
        typed_data: typed,
    })
}

fn provider_uint(value: &Value) -> Result<U256> {
    if let Some(v) = value.as_u64() {
        return Ok(U256::from(v));
    }
    let v = value
        .as_str()
        .ok_or_else(|| GoFinError::validation("Invalid exact provider integer."))?;
    if v.starts_with("0x") {
        return rpc_u256(value);
    }
    u256(&Quantity::new(v)?)
}
fn provider_u64(value: &Value) -> Result<u64> {
    provider_uint(value)?
        .try_into()
        .map_err(|_| GoFinError::validation("Provider integer exceeds uint64."))
}
