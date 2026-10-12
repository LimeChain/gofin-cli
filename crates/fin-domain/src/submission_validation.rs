//! In-process handoff of a final revalidation; never accepted from CLI input.
use crate::{CheckStatus, FinError, PreparedOperation, Result, clock::Clock};
use chrono::DateTime;
use std::sync::Arc;

/// The complete immutable candidate after adapter revalidation and engine
/// binding, valuation and policy checks. This is deliberately not serializable:
/// journal recovery must perform fresh revalidation, never restore this handoff.
pub struct RevalidatedOperation {
    prepared: PreparedOperation,
    clock: Arc<dyn Clock>,
}

impl RevalidatedOperation {
    /// Trusted broker/adapter contract: call only after final revalidation and
    /// all applicable binding checks. Construction grants no financial authority;
    /// the submission journal independently checks it at signing and dispatch.
    pub fn after_revalidation(prepared: PreparedOperation, clock: Arc<dyn Clock>) -> Result<Self> {
        let validated = Self { prepared, clock };
        validated.assert_fresh()?;
        Ok(validated)
    }

    pub fn prepared(&self) -> &PreparedOperation {
        &self.prepared
    }

    /// Cheap local checks after intervening awaits. Reuse never extends either
    /// the candidate lifetime or the original pricing evidence lifetime.
    pub fn assert_fresh(&self) -> Result<()> {
        let now = self.clock.now();
        crate::value_bounds::validate_at(&self.prepared, now)?;
        let expires = DateTime::parse_from_rfc3339(&self.prepared.expires_at)
            .map_err(|_| FinError::validation("Revalidated candidate expiry is invalid."))?;
        if expires <= now
            || self
                .prepared
                .checks
                .iter()
                .any(|c| c.status == CheckStatus::Failed)
        {
            return Err(FinError::new(
                "PREVIEW_EXPIRED",
                "submission",
                "The revalidated candidate is no longer executable; nothing was dispatched.",
                "Prepare a fresh candidate within the existing authority if it remains active.",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FinancialIntent, clock::SystemClock};
    use chrono::{Duration, Utc};
    use serde_json::json;
    use std::sync::Mutex;

    struct MovingClock(Mutex<chrono::DateTime<Utc>>);
    impl Clock for MovingClock {
        fn now(&self) -> chrono::DateTime<Utc> {
            *self.0.lock().unwrap()
        }
    }

    #[test]
    fn handoff_keeps_the_candidate_deadline_and_checks_the_injected_clock() {
        let now = SystemClock.now();
        let clock = Arc::new(MovingClock(Mutex::new(now)));
        let prepared = PreparedOperation {
            operation: "synthetic.transfer".into(),
            input: json!({}),
            binding: json!({}),
            summary: "Synthetic".into(),
            checks: vec![],
            warnings: vec![],
            observed_at: now.to_rfc3339(),
            expires_at: (now + Duration::seconds(30)).to_rfc3339(),
            valuation: None,
            intent: FinancialIntent::default(),
            operation_version: 1,
            action_deadline: None,
        };
        let validated =
            RevalidatedOperation::after_revalidation(prepared.clone(), clock.clone()).unwrap();
        assert_eq!(validated.prepared(), &prepared);
        *clock.0.lock().unwrap() += Duration::seconds(30);
        assert_eq!(
            validated.assert_fresh().unwrap_err().code,
            "PREVIEW_EXPIRED"
        );
    }
}
