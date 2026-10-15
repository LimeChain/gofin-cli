//! Protected Coinbase composition. Public inputs select enrolled identities,
//! never credentials, authentication algorithms or transport origins.
use crate::{BrokerConfigV2, BrokerCredentialProvider, ConnectionConfig};
use fin_coinbase::{CoinbaseAccount, CoinbaseProduct, CoinbaseProfile};
use fin_domain::{FinError, Result};
use fin_exchange::coinbase::{CoinbaseApi, CoinbaseAuth, CoinbaseTransport};
use std::{collections::BTreeMap, sync::Arc};

pub fn profile(
    config: &BrokerConfigV2,
    credentials: Arc<dyn BrokerCredentialProvider>,
    prices: Arc<dyn fin_pricing::UsdPriceSource>,
) -> Result<CoinbaseProfile> {
    let mut accounts = Vec::new();
    let mut transports: BTreeMap<String, Arc<dyn CoinbaseApi>> = BTreeMap::new();
    for connection in &config.connections {
        let (product, name, resource, credential_ref, physical_identity, verified, portfolio_id) =
            match connection {
                ConnectionConfig::Coinbase {
                    name,
                    resource,
                    credential_ref,
                    physical_identity,
                    physical_identity_verified,
                    portfolio_id,
                } => (
                    CoinbaseProduct::Advanced,
                    name,
                    resource,
                    credential_ref,
                    physical_identity,
                    *physical_identity_verified,
                    portfolio_id,
                ),
                ConnectionConfig::CoinbaseExchange {
                    name,
                    resource,
                    credential_ref,
                    physical_identity,
                    physical_identity_verified,
                    portfolio_id,
                } => (
                    CoinbaseProduct::Exchange,
                    name,
                    resource,
                    credential_ref,
                    physical_identity,
                    *physical_identity_verified,
                    portfolio_id,
                ),
                ConnectionConfig::CoinbaseInternational {
                    name,
                    resource,
                    credential_ref,
                    physical_identity,
                    physical_identity_verified,
                    portfolio_id,
                } => (
                    CoinbaseProduct::International,
                    name,
                    resource,
                    credential_ref,
                    physical_identity,
                    *physical_identity_verified,
                    portfolio_id,
                ),
                _ => continue,
            };
        let auth =
            credentials.coinbase_auth(&fin_credentials::CredentialRef::new(credential_ref)?)?;
        let matching_product = matches!(
            (&auth, product),
            (
                CoinbaseAuth::CdpEd25519 { .. } | CoinbaseAuth::CdpEcdsa { .. },
                CoinbaseProduct::Advanced
            ) | (CoinbaseAuth::ExchangeHmac { .. }, CoinbaseProduct::Exchange)
                | (
                    CoinbaseAuth::InternationalHmac { .. },
                    CoinbaseProduct::International
                )
        );
        if !matching_product {
            return Err(FinError::new(
                "COINBASE_CREDENTIAL_PRODUCT_MISMATCH",
                "configuration",
                "The protected credential belongs to a different Coinbase product.",
                "Repair the product connection through protected enrollment.",
            ));
        }
        let transport = CoinbaseTransport::new(auth).map_err(|_| {
            FinError::new(
                "COINBASE_TRANSPORT_UNAVAILABLE",
                "configuration",
                "The protected Coinbase transport could not initialize.",
                "Check the credential format and original connection before retrying.",
            )
        })?;
        if transports
            .insert(credential_ref.clone(), Arc::new(transport))
            .is_some()
        {
            return Err(FinError::validation(
                "A Coinbase credential cannot be rebound to multiple portfolio connections.",
            ));
        }
        accounts.push(CoinbaseAccount {
            name: name.clone(),
            resource: resource.clone(),
            credential_ref: credential_ref.clone(),
            physical_identity: physical_identity.clone(),
            identity_verified: verified,
            portfolio_id: portfolio_id.clone(),
            product,
        });
    }
    Ok(CoinbaseProfile::new(accounts, transports)?.with_prices(prices))
}
