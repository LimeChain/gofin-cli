//! Fixed, request-local diagnostics for bounded read-only evidence collection.
//! Observation never changes authority, scheduling, deadlines or error results.
use serde::Serialize;
use std::{
    collections::BTreeMap,
    future::Future,
    sync::{Arc, Mutex},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    BridgeAllowance,
    BridgeQuotePricing,
    BridgeCapPricing,
    BridgeDeployments,
    BridgeApprovalSimulation,
    BridgeTransactionPreparation,
    BridgeGasPricing,
    BridgePolicyValuation,
    EvmChainIdentity,
    EvmSimulation,
    EvmGasEstimate,
    EvmFees,
    EvmNonce,
    EvmBalance,
    EvmRollupFees,
}

#[derive(Clone, Default)]
pub struct Observation(Arc<Mutex<BTreeMap<Phase, usize>>>);

tokio::task_local! {
    static OBSERVATION: Observation;
}

impl Observation {
    pub async fn observe<F: Future>(&self, work: F) -> F::Output {
        OBSERVATION.scope(self.clone(), work).await
    }

    pub fn pending(&self) -> Vec<Phase> {
        self.0
            .lock()
            .map(|phases| phases.keys().copied().collect())
            .unwrap_or_default()
    }
}

struct Guard {
    observation: Observation,
    phase: Phase,
}
impl Drop for Guard {
    fn drop(&mut self) {
        if let Ok(mut phases) = self.observation.0.lock()
            && let Some(count) = phases.get_mut(&self.phase)
        {
            *count -= 1;
            if *count == 0 {
                phases.remove(&self.phase);
            }
        }
    }
}

pub async fn during<F: Future>(phase: Phase, work: F) -> F::Output {
    let _guard = OBSERVATION
        .try_with(|observation| {
            if let Ok(mut phases) = observation.0.lock() {
                *phases.entry(phase).or_default() += 1;
                Some(Guard {
                    observation: observation.clone(),
                    phase,
                })
            } else {
                None
            }
        })
        .ok()
        .flatten();
    work.await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn repeated_phases_remain_pending_until_every_read_finishes() {
        let observation = Observation::default();
        observation
            .observe(during(Phase::EvmSimulation, async {
                during(Phase::EvmSimulation, async {
                    assert_eq!(observation.pending(), vec![Phase::EvmSimulation]);
                })
                .await;
                assert_eq!(observation.pending(), vec![Phase::EvmSimulation]);
            }))
            .await;
        assert!(observation.pending().is_empty());
        assert_eq!(during(Phase::EvmBalance, async { 7 }).await, 7);
        assert!(observation.pending().is_empty());
    }
}
