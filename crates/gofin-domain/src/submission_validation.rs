//! In-process handoff of a final revalidation; never accepted from CLI input.
use crate::{
    CheckStatus, GoFinError, PreparedOperation, Result, Submission, VerifiedEffects, clock::Clock,
    verified_effect_digest,
};
use chrono::DateTime;
use std::sync::Arc;

/// Adapter-verified proof that an expired candidate did not dispatch. The
/// adapter owns interpretation of venue evidence; construction binds its
/// attestation to the durable submission and canonical effect proof.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedNonDispatchExpiry {
    operation_id: String,
    submission: Submission,
    kind: CandidateExpiryKind,
    effects_digest: String,
}

/// Generic expiry class. Venue-specific error codes remain adapter-owned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CandidateExpiryKind {
    Quote,
    TransactionLifetime,
}

impl VerifiedNonDispatchExpiry {
    pub fn from_verified_effects(
        operation_id: &str,
        submission: &Submission,
        verified: &VerifiedEffects,
        kind: CandidateExpiryKind,
    ) -> Result<Self> {
        if operation_id.is_empty()
            || &verified.submission != submission
            || !verified.effects.is_empty()
            || verified_effect_digest(verified)? != verified.proof_digest
        {
            return Err(invalid_non_dispatch_expiry());
        }
        Ok(Self {
            operation_id: operation_id.to_owned(),
            submission: submission.clone(),
            kind,
            effects_digest: verified.proof_digest.clone(),
        })
    }

    pub fn operation_id(&self) -> &str {
        &self.operation_id
    }

    pub fn submission(&self) -> &Submission {
        &self.submission
    }

    pub fn kind(&self) -> CandidateExpiryKind {
        self.kind
    }

    pub fn effects_digest(&self) -> &str {
        &self.effects_digest
    }
}

fn invalid_non_dispatch_expiry() -> GoFinError {
    GoFinError::new(
        "NON_DISPATCH_EXPIRY_INVALID",
        "reconciliation",
        "The adapter did not provide valid verified evidence for a non-dispatched expired transaction.",
        "Keep the original attempt unresolved and reconcile its persisted submission.",
    )
}

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
            .map_err(|_| GoFinError::validation("Revalidated candidate expiry is invalid."))?;
        if expires <= now
            || self
                .prepared
                .checks
                .iter()
                .any(|c| c.status == CheckStatus::Failed)
        {
            return Err(GoFinError::new(
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
    use crate::{
        Amount, AssetId, FinancialEffect, FinancialIntent, Quantity, ResourceId, clock::SystemClock,
    };
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

    #[test]
    fn non_dispatch_expiry_attestation_binds_empty_canonical_effects() {
        let submission = Submission {
            kind: "synthetic.transaction".into(),
            reference: "synthetic-reference".into(),
            data: json!({"message":"immutable"}),
        };
        let mut effects = VerifiedEffects {
            evidence_version: 1,
            submission: submission.clone(),
            effects: Vec::new(),
            evidence: json!({"adapter_verified":"not_dispatched"}),
            observed_at: "2026-10-13T00:00:00Z".into(),
            proof_digest: String::new(),
        };
        effects.proof_digest = verified_effect_digest(&effects).unwrap();
        let attestation = VerifiedNonDispatchExpiry::from_verified_effects(
            "attempt-1",
            &submission,
            &effects,
            CandidateExpiryKind::TransactionLifetime,
        )
        .unwrap();
        assert_eq!(attestation.operation_id(), "attempt-1");
        assert_eq!(attestation.submission(), &submission);
        assert_eq!(attestation.kind(), CandidateExpiryKind::TransactionLifetime);
        assert_eq!(attestation.effects_digest(), effects.proof_digest);

        let mut tampered = effects.clone();
        tampered.evidence["adapter_verified"] = json!("unknown");
        assert!(
            VerifiedNonDispatchExpiry::from_verified_effects(
                "attempt-1",
                &submission,
                &tampered,
                CandidateExpiryKind::TransactionLifetime,
            )
            .is_err()
        );
        assert!(
            VerifiedNonDispatchExpiry::from_verified_effects(
                "attempt-1",
                &Submission {
                    reference: "other-reference".into(),
                    ..submission.clone()
                },
                &effects,
                CandidateExpiryKind::Quote,
            )
            .is_err()
        );
        let mut financial = effects.clone();
        financial.effects.push(FinancialEffect::CashDebit {
            resource: ResourceId("synthetic-wallet".into()),
            amount: Amount {
                asset: AssetId {
                    network: "synthetic".into(),
                    reference: "TOKEN".into(),
                    decimals: 0,
                },
                atoms: Quantity::from_u64(1),
            },
        });
        financial.proof_digest = verified_effect_digest(&financial).unwrap();
        assert!(
            VerifiedNonDispatchExpiry::from_verified_effects(
                "attempt-1",
                &submission,
                &financial,
                CandidateExpiryKind::TransactionLifetime,
            )
            .is_err()
        );
    }
}
