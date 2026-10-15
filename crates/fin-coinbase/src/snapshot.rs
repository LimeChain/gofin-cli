//! Compose independently scoped product observations without merging custody identities.
use crate::CoinbaseProfile;
use fin_domain::{FinancialState, Result, SnapshotScope, StateSnapshot};
use fin_exchange::ExchangeProfile;
use std::collections::BTreeSet;

pub(crate) async fn mixed_product_snapshot(
    profile: &CoinbaseProfile,
    scope: Option<&SnapshotScope>,
) -> Result<Option<StateSnapshot>> {
    let full = SnapshotScope::default();
    let scope = scope.unwrap_or(&full);
    let selected = profile
        .accounts()
        .iter()
        .filter(|account| {
            scope.matches_resource(&account.resource)
                && scope.matches_network(account.product.provider())
        })
        .collect::<Vec<_>>();
    let products = selected
        .iter()
        .map(|account| account.product)
        .collect::<BTreeSet<_>>();
    if products.len() < 2 {
        return Ok(None);
    }
    let mut result = StateSnapshot {
        state: FinancialState::default(),
        observed_at: fin_domain::now(),
        complete: !scope.is_full(),
        coverage: vec![],
        pending: vec![],
    };
    // Explicit selectors that match no enrolled account cannot become complete
    // merely because the remaining product observations succeeded.
    result.complete &= scope
        .resources
        .iter()
        .all(|resource| selected.iter().any(|account| &account.resource == resource))
        && scope.networks.iter().all(|network| {
            selected
                .iter()
                .any(|account| account.product.provider() == network)
        })
        && scope.assets.iter().all(|asset| {
            selected
                .iter()
                .any(|account| account.product.provider() == asset.network)
        });
    for product in products {
        let product_scope = SnapshotScope {
            networks: vec![product.provider().into()],
            resources: selected
                .iter()
                .filter(|account| account.product == product)
                .map(|account| account.resource.clone())
                .collect(),
            assets: scope
                .assets
                .iter()
                .filter(|asset| asset.network == product.provider())
                .cloned()
                .collect(),
        };
        // Trait calls are boxed and this explicit product scope contains only
        // one provider, so it cannot enter this composition branch again.
        let observed = ExchangeProfile::snapshot_scoped(profile, &product_scope).await?;
        result.complete &= observed.complete;
        if observed.observed_at < result.observed_at {
            result.observed_at = observed.observed_at;
        }
        result.state.holdings.extend(observed.state.holdings);
        result.state.positions.extend(observed.state.positions);
        result.state.authorities.extend(observed.state.authorities);
        result.state.in_flight.extend(observed.state.in_flight);
        result.coverage.extend(observed.coverage);
        result.pending.extend(observed.pending);
    }
    if scope.is_full() {
        result.coverage.push("Mixed Coinbase full financial-state coverage remains incomplete for unmodeled lending and derivatives.".into());
    }
    result.coverage.sort();
    result.coverage.dedup();
    result.pending.sort();
    result.pending.dedup();
    Ok(Some(result))
}
