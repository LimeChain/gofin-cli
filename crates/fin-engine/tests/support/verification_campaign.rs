//! Synthetic rehearsal guards, compiled only by tests or a private harness.
//! These do not change the shipped CLI or authorize any financial action.
use num_bigint::BigUint;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

pub struct CampaignLock(PathBuf);
impl CampaignLock {
    pub fn acquire(path: impl AsRef<Path>) -> Result<Self, &'static str> {
        let path = path.as_ref().to_path_buf();
        fs::create_dir(&path).map_err(|_| "CAMPAIGN_LOCKED")?;
        Ok(Self(path))
    }
}
impl Drop for CampaignLock {
    fn drop(&mut self) {
        let _ = fs::remove_dir(&self.0);
    }
}

/// A crash anywhere after this marker requires status-only recovery. No reset
/// or timeout can grant another dispatch for this authorized attempt.
pub fn claim_dispatch(path: impl AsRef<Path>) -> Result<(), &'static str> {
    let path = path.as_ref();
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options
        .open(path)
        .map_err(|_| "CAMPAIGN_DISPATCH_ALREADY_CLAIMED")?;
    file.write_all(b"explicit-human-authorized-new-attempt; never replay\n")
        .and_then(|_| file.sync_all())
        .map_err(|_| "CAMPAIGN_DISPATCH_MARKER_FAILED")?;
    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        fs::File::open(parent)
            .and_then(|dir| dir.sync_all())
            .map_err(|_| "CAMPAIGN_DISPATCH_MARKER_FAILED")?;
    }
    Ok(())
}

/// The quoted new debit prices the unresolved original proportionately and
/// rounds upward. Uncertain principal is counted even when still visible in
/// the account; no debt or possible credit reduces the required reserve.
pub fn enforce_risk(
    new_usd: &BigUint,
    old_atoms: &BigUint,
    new_atoms: &BigUint,
    venue_cap: &BigUint,
    other_allocation: &BigUint,
    total_cap: &BigUint,
) -> Result<BigUint, &'static str> {
    if new_usd == &BigUint::from(0u8) || new_atoms == &BigUint::from(0u8) {
        return Err("CAMPAIGN_RISK_INVALID");
    }
    let old_usd = (new_usd * old_atoms + new_atoms - BigUint::from(1u8)) / new_atoms;
    let combined = &old_usd + new_usd;
    if &combined > venue_cap || &combined + other_allocation > *total_cap {
        return Err("CAMPAIGN_RISK_LIMIT");
    }
    Ok(combined)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn risk(
        v: u64,
        old: u64,
        new: u64,
        venue: u64,
        other: u64,
        total: u64,
    ) -> Result<BigUint, &'static str> {
        enforce_risk(
            &v.into(),
            &old.into(),
            &new.into(),
            &venue.into(),
            &other.into(),
            &total.into(),
        )
    }
    #[test]
    fn old_reserve_remains_charged() {
        assert_eq!(
            risk(4_900_000, 50, 49, 10_000_000, 69_000_000, 79_000_000).unwrap(),
            BigUint::from(9_900_000u64)
        );
        assert_eq!(
            risk(5_000_001, 50, 50, 10_000_000, 69_000_000, 79_000_000),
            Err("CAMPAIGN_RISK_LIMIT")
        );
    }
    #[test]
    fn quote_rounding_and_zero_are_conservative() {
        assert_eq!(risk(1, 2, 3, 2, 0, 2).unwrap(), BigUint::from(2u8));
        assert_eq!(risk(0, 2, 3, 2, 0, 2), Err("CAMPAIGN_RISK_INVALID"));
        assert_eq!(risk(1, 2, 0, 2, 0, 2), Err("CAMPAIGN_RISK_INVALID"));
        assert_eq!(risk(1, 2, 3, 2, 2, 3), Err("CAMPAIGN_RISK_LIMIT"));
    }
    #[test]
    fn lock_and_dispatch_survive_independent_attempts() {
        let dir = std::env::temp_dir().join(format!(
            "fin-synthetic-campaign-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&dir).unwrap();
        let first = CampaignLock::acquire(dir.join("account.lock")).unwrap();
        assert!(CampaignLock::acquire(dir.join("account.lock")).is_err());
        claim_dispatch(dir.join("step.claimed")).unwrap();
        drop(first);
        let second = CampaignLock::acquire(dir.join("account.lock")).unwrap();
        assert!(claim_dispatch(dir.join("step.claimed")).is_err());
        drop(second);
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn concurrent_dispatch_claims_allow_one_attempt() {
        let dir = std::env::temp_dir().join(format!(
            "fin-synthetic-claim-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&dir).unwrap();
        let path = dir.join("one-dispatch.claimed");
        let handles: Vec<_> = (0..16)
            .map(|_| {
                let path = path.clone();
                std::thread::spawn(move || claim_dispatch(path).is_ok())
            })
            .collect();
        assert_eq!(
            handles
                .into_iter()
                .map(|h| h.join().unwrap())
                .filter(|won| *won)
                .count(),
            1
        );
        // Re-open after a completed claim also refuses; neither expiry nor a
        // fresh process/attempt grants the same financial dispatch again.
        assert!(claim_dispatch(&path).is_err());
        fs::remove_dir_all(dir).unwrap();
    }
}
