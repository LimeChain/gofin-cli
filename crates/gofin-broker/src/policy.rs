//! Compatibility rejection for retired personal policy setters.
use crate::{BrokerConfigV2, Store};
use gofin_discovery::PolicySetArgs;
use gofin_domain::{AuthorizationChallenge, GoFinError, Result};
use serde_json::Value;
use std::future::Future;

pub(super) async fn update<A, F, S>(
    _config: &BrokerConfigV2,
    _store: &Store,
    _args: &PolicySetArgs,
    _approve: A,
    _save: S,
) -> Result<Value>
where
    A: FnOnce(Value) -> F,
    F: Future<Output = Result<AuthorizationChallenge>>,
    S: FnOnce(&BrokerConfigV2) -> Result<()>,
{
    Err(GoFinError::new(
        "PERSONAL_LIMITS_RETIRED",
        "configuration",
        "Personal USD limits and the unpriced-asset exception have been retired.",
        "Approve native amounts, destinations, outcome bounds and fees for each action. No setting was changed.",
    ))
}
